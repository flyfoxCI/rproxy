"""Build and test Compose with isolated configs, local fixtures and an owned disposable volume."""
import argparse
import base64
import http.client
import json
import os
from pathlib import Path
import secrets
import shutil
import socket
import subprocess
import tempfile
import time

ROOT = Path(__file__).resolve().parents[1]


def free_port():
    with socket.socket() as server:
        server.bind(('127.0.0.1', 0))
        return server.getsockname()[1]


def proxy_request(port, username, password):
    credentials = base64.b64encode(f'{username}:{password}'.encode()).decode()
    connection = http.client.HTTPConnection('127.0.0.1', port, timeout=10)
    try:
        connection.request('GET', 'http://fixture:18080/payload', headers={
            'Proxy-Authorization': 'Basic ' + credentials, 'Connection': 'close'})
        response = connection.getresponse()
        return response.status, response.read()
    finally:
        connection.close()


def receive(stream, size):
    data = b''
    while len(data) < size:
        chunk = stream.recv(size - len(data))
        if not chunk:
            raise AssertionError('SOCKS connection closed early')
        data += chunk
    return data


def socks_request(port, username, password):
    with socket.create_connection(('127.0.0.1', port), timeout=10) as stream:
        stream.sendall(b'\x05\x01\x02')
        assert receive(stream, 2) == b'\x05\x02', 'SOCKS authentication not negotiated'
        user, secret = username.encode(), password.encode()
        stream.sendall(b'\x01' + bytes([len(user)]) + user + bytes([len(secret)]) + secret)
        assert receive(stream, 2) == b'\x01\x00', 'SOCKS authentication failed'
        stream.sendall(b'\x05\x01\x00\x03\x07fixture' + (18080).to_bytes(2, 'big'))
        header = receive(stream, 4)
        assert header[:3] == b'\x05\x00\x00', f'SOCKS connect failed: {header!r}'
        size = {1: 4, 4: 16}.get(header[3])
        if size is None:
            assert header[3] == 3, 'Unknown SOCKS bound address type'
            size = receive(stream, 1)[0]
        receive(stream, size + 2)
        stream.sendall(b'GET /payload HTTP/1.1\r\nHost: fixture:18080\r\nConnection: close\r\n\r\n')
        response = http.client.HTTPResponse(stream)
        response.begin()
        assert response.status == 200 and response.read() == b'mounted-compose-ok'


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--engine', choices=('docker', 'podman'), default='docker' if shutil.which('docker') else 'podman')
    parser.add_argument('--skip-build', action='store_true')
    parser.add_argument('--image-prefix', help='Reuse images from this Compose project (with --skip-build)')
    args = parser.parse_args()
    if args.image_prefix and not args.skip_build:
        parser.error('--image-prefix requires --skip-build')
    project = 'rproxy-smoke-' + secrets.token_hex(4)
    prefix = args.image_prefix or project
    password = secrets.token_urlsafe(24)
    token = secrets.token_urlsafe(32)
    http_port, socks_port = free_port(), free_port()
    while socks_port == http_port:
        socks_port = free_port()
    environment = {key: value for key, value in os.environ.items() if not key.startswith(('RPROXY_', 'COMPOSE_'))}
    # Mount existing files, not the user's actual configuration or secrets.
    with tempfile.TemporaryDirectory(prefix='rproxy-compose-') as directory:
        directory = Path(directory)
        config = directory / 'rproxy.toml'
        config.write_text((ROOT / 'rproxy.example.toml').read_text(encoding='utf-8').replace(
            'https://api.ipify.org/', 'http://fixture:18080/ip'), encoding='utf-8')
        collector = directory / 'proxy.toml'
        collector.write_text('''[rproxy]
url = "http://rproxy:9100"
[[providers]]
name = "local-fixture"
url = "http://fixture:18080/supplier"
interval_seconds = 2
timeout_seconds = 2
attempts = 1
''', encoding='utf-8')
        providers = directory / 'providers.env'
        providers.write_text('# No external supplier credentials used.\n', encoding='utf-8')
        env_file = directory / '.env'
        env_file.write_text(f'RPROXY_PASSWORD={password}\nRPROXY_ADMIN_TOKEN={token}\n', encoding='utf-8')
        environment.update({
            'RPROXY_CONFIG': config.as_posix(), 'RPROXY_COLLECTOR_CONFIG': collector.as_posix(),
            'RPROXY_PROVIDERS_ENV': providers.as_posix(), 'RPROXY_BIND_ADDRESS': '127.0.0.1',
            'RPROXY_HTTP_PORT': str(http_port), 'RPROXY_SOCKS5_PORT': str(socks_port),
        })
        override = directory / 'smoke.json'
        override.write_text(json.dumps({'services': {
            'rproxy': {'image': f'{prefix}-rproxy'},
            'collector': {'image': f'{prefix}-collector'},
            'fixture': {
                'image': f'{prefix}-collector',
                'entrypoint': ['python', '-I', '/fixture.py'], 'command': [],
                'read_only': True, 'cap_drop': ['ALL'],
                'volumes': [{'type': 'bind', 'source': (ROOT / 'scripts/deployment_tests/fixture.py').as_posix(),
                             'target': '/fixture.py', 'read_only': True,
                             'bind': {'create_host_path': False, 'selinux': 'Z'}}],
            },
        }}), encoding='utf-8')
        command = [args.engine, 'compose', '-p', project, '--env-file', str(env_file),
                   '-f', str(ROOT / 'compose.yaml'), '-f', str(override)]

        def compose(*arguments, check=True, env=None):
            result = subprocess.run(command + list(arguments), cwd=ROOT, env=env or environment,
                                    text=True, encoding='utf-8', errors='replace', capture_output=True)
            if check and result.returncode:
                raise RuntimeError(f'Compose {arguments[0]} failed:\n{result.stdout}\n{result.stderr}')
            return result

        def status():
            return json.loads(compose('exec', '-T', 'rproxy', 'rproxy', '--config', '/etc/rproxy/rproxy.toml',
                                      '--admin-bind', '127.0.0.1:9100', 'healthcheck').stdout)

        def wait_ready(predicate, label):
            deadline = time.monotonic() + 120
            last_error = None
            while time.monotonic() < deadline:
                try:
                    if predicate():
                        return
                except (RuntimeError, ValueError, OSError) as error:
                    last_error = error
                time.sleep(1)
            raise AssertionError(f'Timed out waiting for {label}: {last_error}')

        try:
            if not args.skip_build:
                print('Building both deployment images', flush=True)
                compose('build')
            compose('config', '--quiet')
            missing_secret = compose('config', '--quiet', check=False, env={**environment, 'RPROXY_PASSWORD': ''})
            assert missing_secret.returncode != 0, 'Empty proxy password was accepted'
            missing = directory / 'missing.toml'
            result = compose('run', '--rm', '--no-deps', 'rproxy', 'check-config', check=False,
                             env={**environment, 'RPROXY_CONFIG': missing.as_posix()})
            assert result.returncode != 0 and not missing.exists(), 'Missing mount was created or accepted'
            print('Starting isolated Compose services and local supplier', flush=True)
            compose('up', '-d', '--no-build')
            wait_ready(lambda: status()['verified'] == 1, 'collector submission and Rust verification')
            collector_id = compose('ps', '-q', 'collector').stdout.strip()
            running = subprocess.run([args.engine, 'inspect', '--format', '{{.State.Running}}', collector_id],
                                     capture_output=True, text=True, check=True).stdout.strip()
            assert running == 'true', 'Collector is not running'
            username = 'v1|alice|shop|compose-smoke|stick'
            assert proxy_request(http_port, username, password) == (200, b'mounted-compose-ok')
            assert proxy_request(http_port, username, 'incorrect-password')[0] == 407
            socks_request(socks_port, username, password)
            assert status()['sessions'] == 1
            print('HTTP, SOCKS5, authentication and supplier collection passed', flush=True)
            compose('stop', 'rproxy')
            compose('start', 'rproxy')
            wait_ready(lambda: status()['sessions'] == 1, 'SQLite session persistence after restart')
            # Atomic editor saves replace inodes; force-recreate must remount the host file.
            replacement = directory / 'edited.toml'
            replacement.write_text(config.read_text(encoding='utf-8').replace('name = "alice"', 'name = "bob"'), encoding='utf-8')
            replacement.replace(config)
            compose('up', '-d', '--no-build', '--force-recreate', 'rproxy', 'collector')
            wait_ready(lambda: status()['verified'] == 1, 'recreated service readiness')
            assert proxy_request(http_port, username, password)[0] == 407, 'Host config edit was not applied'
            assert proxy_request(http_port, 'v1|bob|news|host-edit|stick', password) == (200, b'mounted-compose-ok')
            # env_file changes require recreation, not a plain restart.
            collector.write_text('[rproxy]\nurl = "http://rproxy:9100"\n', encoding='utf-8')
            compose('up', '-d', '--no-build', '--force-recreate', 'collector')
            wait_ready(lambda: 'No enabled providers; collector is idle' in
                       compose('logs', '--no-color', 'collector').stdout, 'idle collector startup')
            print('PASS: mounted host config edits, idle collector and durable state verified', flush=True)
        except Exception:
            # These are disposable generated credentials; still redact them from failure logs.
            logs = compose('logs', '--no-color', '--tail', '80', check=False)
            print((logs.stdout + logs.stderr).replace(password, '<redacted>').replace(token, '<redacted>'), flush=True)
            raise
        finally:
            result = compose('down', '--volumes', '--remove-orphans', check=False)
            if result.returncode:
                raise RuntimeError(f'Could not clean up owned Compose project {project}: {result.stderr}')


if __name__ == '__main__':
    main()
