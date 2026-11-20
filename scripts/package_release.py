"""Package both target-specific release executables (Python 3.11+)."""
import argparse
import hashlib
import json
import os
import pathlib
import secrets
import tempfile
import time
import re
import subprocess
import tarfile
import tomllib
import zipfile

TARGETS = {
    'x86_64-pc-windows-msvc',
    'x86_64-unknown-linux-musl',
    'aarch64-unknown-linux-musl',
    'x86_64-apple-darwin',
    'aarch64-apple-darwin',
}


def build_archive(root, target, tag, output):
    if target not in TARGETS or not re.fullmatch(r'v\d+\.\d+\.\d+', tag):
        raise ValueError('unsupported target or invalid release tag')
    version = tomllib.loads((root / 'Cargo.toml').read_text(encoding='utf-8'))['package']['version']
    if tag != 'v' + version:
        raise ValueError('release tag must match Cargo.toml version')
    windows = 'windows' in target
    name = 'rproxy.exe' if windows else 'rproxy'
    executable = root / 'target' / target / 'release' / name
    cli_name = 'rproxy-cli.exe' if windows else 'rproxy-cli'
    cli = executable.with_name(cli_name)
    for binary in (executable, cli):
        if not binary.is_file():
            raise FileNotFoundError(binary)
    files = [(executable, name), (cli, cli_name), (root / 'README.md', 'README.md'),
             (root / 'docs/rproxy-cli.md', 'docs/rproxy-cli.md'),
             (root / 'docs/collector-management.md', 'docs/collector-management.md'),
             (root / 'rproxy.example.toml', 'rproxy.example.toml'),
             (root / 'sources.example.toml', 'sources.example.toml'),
             (root / 'scripts/requirements.txt', 'scripts/requirements.txt'),
             (root / 'scripts/proxy.example.toml', 'scripts/proxy.example.toml'),
             (root / 'scripts/providers.env.example', 'scripts/providers.env.example')]
    files.extend((path, 'scripts/' + path.name) for path in sorted((root / 'scripts').glob('*.py')))
    for path, _ in files:
        if not path.is_file() or path.is_symlink():
            raise ValueError('release input is missing or a symbolic link: ' + str(path))
    output.mkdir(parents=True, exist_ok=True)
    prefix = f'rproxy-{tag}-{target}'
    archive = output / (prefix + ('.zip' if windows else '.tar.gz'))
    if windows:
        with zipfile.ZipFile(archive, 'w', compression=zipfile.ZIP_DEFLATED) as bundle:
            for path, relative in files:
                bundle.write(path, prefix + '/' + relative)
    else:
        with tarfile.open(archive, 'w:gz') as bundle:
            for path, relative in files:
                info = bundle.gettarinfo(str(path), prefix + '/' + relative)
                info.mode = 0o755 if relative in (name, cli_name) else 0o644
                info.uid = info.gid = 0
                info.uname = info.gname = ''
                with path.open('rb') as source:
                    bundle.addfile(info, source)
    return archive


def native_management_smoke(server, cli):
    """Check real SQLite startup and online CLI on this native host, using only loopback."""
    server, cli = server.resolve(), cli.resolve()
    environment = {key: value for key, value in os.environ.items() if not key.startswith('RPROXY_')}
    environment.update(RPROXY_ADMIN_TOKEN=secrets.token_urlsafe(32), RUST_LOG='info')
    with tempfile.TemporaryDirectory(prefix='rproxy-native-gate-') as directory:
        directory = pathlib.Path(directory)
        config = directory / 'rproxy.toml'
        config.write_text('database="state.sqlite3"\nhttp_bind="127.0.0.1:0"\n[admin]\nbind="127.0.0.1:0"\n[auth]\nmode="required"\n[probe]\nenabled=false\n', encoding='utf-8')
        log_path = directory / 'server.log'
        with log_path.open('w', encoding='utf-8') as log:
            process = subprocess.Popen([str(server), '--config', str(config), '--no-socks5'],
                                       cwd=directory, env=environment, stdout=log, stderr=log)
            try:
                deadline = time.monotonic() + 30
                while True:
                    if process.poll() is not None:
                        details = log_path.read_text(encoding='utf-8', errors='replace')[-4096:]
                        details = details.replace(environment['RPROXY_ADMIN_TOKEN'], '<redacted>')
                        raise ValueError(f'native SQLite/server startup check failed (exit {process.poll()}):\n{details}')
                    match = re.search(r'listening protocol=admin address=127\.0\.0\.1:(\d+)', log_path.read_text(encoding='utf-8', errors='replace'))
                    if match:
                        break
                    if time.monotonic() >= deadline:
                        raise ValueError('native management listener startup timed out')
                    time.sleep(0.05)
                base = [str(cli), '--endpoint', 'http://127.0.0.1:' + match[1], '--json']
                password = secrets.token_urlsafe(24)
                created = subprocess.run(base + ['user', 'add', 'packaging-check', '--business', 'packaging', '--password-stdin'],
                                         input=password + '\n', text=True, capture_output=True, env=environment, timeout=20)
                if created.returncode or json.loads(created.stdout).get('name') != 'packaging-check':
                    raise ValueError('native CLI online user creation failed')
                listed = subprocess.run(base + ['user', 'list'], text=True, capture_output=True, env=environment, timeout=15)
                if listed.returncode or not any(user.get('name') == 'packaging-check' for user in json.loads(listed.stdout).get('items', [])):
                    raise ValueError('native CLI durable user read failed')
            finally:
                process.terminate()
                try:
                    process.wait(timeout=10)
                except subprocess.TimeoutExpired:
                    process.kill()
                    process.wait(timeout=10)


def package_native(root, target, tag, output):
    """Validate trusted local binaries; diagnostic refs can never publish archives."""
    diagnostic = tag.startswith('diagnostic/')
    if target not in TARGETS or (not diagnostic and not re.fullmatch(r'v\d+\.\d+\.\d+', tag)):
        raise ValueError('unsupported target or invalid release tag')
    version = tomllib.loads((root / 'Cargo.toml').read_text(encoding='utf-8'))['package']['version']
    if not diagnostic and tag != 'v' + version:
        raise ValueError('release tag must match Cargo.toml version')
    name = 'rproxy-cli.exe' if 'windows' in target else 'rproxy-cli'
    binary = root / 'target' / target / 'release' / name
    if not binary.is_file() or binary.is_symlink():
        raise ValueError('native CLI must be a regular locally built file')
    try:
        result = subprocess.run([str(binary), '--version'], capture_output=True,
                                text=True, timeout=10, check=False)
    except (OSError, subprocess.TimeoutExpired) as error:
        raise ValueError('native CLI --version startup check failed') from error
    if result.returncode != 0 or result.stdout.strip() != 'rproxy-cli ' + version:
        raise ValueError('native CLI --version must match Cargo.toml version')
    server_name = 'rproxy.exe' if 'windows' in target else 'rproxy'
    server = binary.with_name(server_name)
    if not server.is_file() or server.is_symlink():
        raise ValueError('native server must be a regular locally built file')
    native_management_smoke(server, binary)
    if diagnostic:
        raise ValueError('diagnostic-only native checks completed; refusing archive creation and publication')
    return build_archive(root, target, tag, output)


def verify_archives(directory, tag):
    if not re.fullmatch(r'v\d+\.\d+\.\d+', tag):
        raise ValueError('invalid release tag')
    expected = {f'rproxy-{tag}-{target}' + ('.zip' if 'windows' in target else '.tar.gz')
                for target in TARGETS}
    if not directory.is_dir():
        raise ValueError('release directory is missing')
    actual = {path.name for path in directory.iterdir() if path.name != 'SHA256SUMS'}
    if actual != expected:
        raise ValueError(f'incomplete or unexpected release archives: missing={sorted(expected - actual)}, '
                         f'unexpected={sorted(actual - expected)}')
    archives = [directory / name for name in sorted(expected)]
    if any(not path.is_file() or path.is_symlink() or path.stat().st_size == 0 for path in archives):
        raise ValueError('release archives must be nonempty regular files')
    for target in TARGETS:
        prefix = f'rproxy-{tag}-{target}'
        windows = 'windows' in target
        names = [prefix + '/' + name + ('.exe' if windows else '')
                 for name in ('rproxy', 'rproxy-cli')]
        try:
            if windows:
                with zipfile.ZipFile(directory / (prefix + '.zip')) as bundle:
                    if any(bundle.getinfo(name).is_dir() or bundle.getinfo(name).file_size == 0
                           for name in names):
                        raise ValueError('release archive must contain both binaries')
            else:
                with tarfile.open(directory / (prefix + '.tar.gz'), 'r:gz') as bundle:
                    if any(not bundle.getmember(name).isfile() or bundle.getmember(name).size == 0
                           or bundle.getmember(name).mode & 0o111 == 0 for name in names):
                        raise ValueError('release archive must contain both binaries as executable files')
        except (KeyError, tarfile.TarError, zipfile.BadZipFile, OSError) as error:
            raise ValueError('release archive must contain both binaries') from error
    return archives


def write_checksums(directory):
    archives = sorted(path for path in directory.iterdir()
                      if path.name.endswith(('.zip', '.tar.gz')) and path.is_file())
    if not archives:
        raise ValueError('no release archives to checksum')
    lines = []
    for path in archives:
        with path.open('rb') as source:
            lines.append(hashlib.file_digest(source, 'sha256').hexdigest() + '  ' + path.name + '\n')
    destination = directory / 'SHA256SUMS'
    destination.write_text(''.join(lines), encoding='utf-8', newline='\n')
    return destination


if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    subparsers = parser.add_subparsers(dest='command', required=True)
    package = subparsers.add_parser('package')
    package.add_argument('--target', choices=sorted(TARGETS), required=True)
    package.add_argument('--tag', required=True)
    package.add_argument('--output', type=pathlib.Path, default=pathlib.Path('dist'))
    verify = subparsers.add_parser('verify')
    verify.add_argument('--tag', required=True)
    verify.add_argument('--directory', type=pathlib.Path, default=pathlib.Path('dist'))
    checksums = subparsers.add_parser('checksums')
    checksums.add_argument('--directory', type=pathlib.Path, default=pathlib.Path('dist'))
    args = parser.parse_args()
    if args.command == 'package':
        root = pathlib.Path(__file__).resolve().parents[1]
        print(package_native(root, args.target, args.tag, args.output))
    elif args.command == 'verify':
        for archive in verify_archives(args.directory, args.tag):
            print(archive)
    else:
        print(write_checksums(args.directory))
