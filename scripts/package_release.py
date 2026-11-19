"""Package a target-specific release executable (Python 3.11+)."""
import argparse
import hashlib
import pathlib
import re
import tarfile
import tomllib
import zipfile

TARGETS = {'x86_64-pc-windows-msvc', 'x86_64-unknown-linux-musl'}


def build_archive(root, target, tag, output):
    if target not in TARGETS or not re.fullmatch(r'v\d+\.\d+\.\d+', tag):
        raise ValueError('unsupported target or invalid release tag')
    version = tomllib.loads((root / 'Cargo.toml').read_text(encoding='utf-8'))['package']['version']
    if tag != 'v' + version:
        raise ValueError('release tag must match Cargo.toml version')
    windows = 'windows' in target
    name = 'rproxy.exe' if windows else 'rproxy'
    executable = root / 'target' / target / 'release' / name
    if not executable.is_file():
        raise FileNotFoundError(executable)
    files = [(executable, name), (root / 'README.md', 'README.md'),
             (root / 'rproxy.example.toml', 'rproxy.example.toml'),
             (root / 'scripts/requirements.txt', 'scripts/requirements.txt')]
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
                info.mode = 0o755 if relative == name else 0o644
                info.uid = info.gid = 0
                info.uname = info.gname = ''
                with path.open('rb') as source:
                    bundle.addfile(info, source)
    return archive


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
    checksums = subparsers.add_parser('checksums')
    checksums.add_argument('--directory', type=pathlib.Path, default=pathlib.Path('dist'))
    args = parser.parse_args()
    if args.command == 'package':
        root = pathlib.Path(__file__).resolve().parents[1]
        print(build_archive(root, args.target, args.tag, args.output))
    else:
        print(write_checksums(args.directory))
