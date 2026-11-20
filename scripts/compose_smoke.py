"""Test managed CLI, collection and mounted Compose with owned disposable resources."""
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


def proxy_request(port, username=None, password=None):
    headers = {'Connection': 'close'}
    if username is not None:
        headers['Proxy-Authorization'] = 'Basic ' + base64.b64encode(f'{username}:{password}'.encode()).decode()
    connection = http.client.HTTPConnection('127.0.0.1', port, timeout=10)
    try:
        connection.request('GET', 'http://fixture:18080/payload', headers=headers)
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
        assert receive(stream, 2) == b'\x05\x02'
        user, secret = username.encode(), password.encode()
        stream.sendall(b'\x01' + bytes([len(user)]) + user + bytes([len(secret)]) + secret)
        assert receive(stream, 2) == b'\x01\x00'
        stream.sendall(b'\x05\x01\x00\x03\x07fixture' + (18080).to_bytes(2, 'big'))
        header = receive(stream, 4)
        assert header[:3] == b'\x05\x00\x00', 'SOCKS connect failed'
        size = {1: 4, 4: 16}.get(header[3])
        if size is None:
            assert header[3] == 3
            size = receive(stream, 1)[0]
        receive(stream, size + 2)
        stream.sendall(b'GET /payload HTTP/1.1\r\nHost: fixture:18080\r\nConnection: close\r\n\r\n')
        response = http.client.HTTPResponse(stream)
        response.begin()
        assert response.status == 200 and response.read() == b'mounted-compose-ok'


def open_tunnel(port, username, password):
    stream = socket.create_connection(('127.0.0.1', port), timeout=10)
    credentials = base64.b64encode(f'{username}:{password}'.encode()).decode()
    stream.sendall(f'CONNECT fixture:18080 HTTP/1.1\r\nHost: fixture:18080\r\nProxy-Authorization: Basic {credentials}\r\n\r\n'.encode())
    header = bytearray()
    while not header.endswith(b'\r\n\r\n'):
        header.extend(receive(stream, 1))
        assert len(header) < 32768
    assert header.startswith(b'HTTP/1.1 200'), 'Tunnel could not be established'
    return stream


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--engine', choices=('docker', 'podman'), default='docker' if shutil.which('docker') else 'podman')
    parser.add_argument('--skip-build', action='store_true')
    parser.add_argument('--image-prefix', help='Reuse this Compose project image prefix with --skip-build')
    args = parser.parse_args()
    if args.image_prefix and not args.skip_build:
        parser.error('--image-prefix requires --skip-build')
    project = 'rproxy-smoke-' + secrets.token_hex(4)
    prefix = args.image_prefix or project
    password, new_password, bob_password = [secrets.token_urlsafe(24) for _ in range(3)]
    token, worker_token = [secrets.token_urlsafe(32) for _ in range(2)]
    http_port, socks_port = free_port(), free_port()
    while socks_port == http_port:
        socks_port = free_port()
    environment = {key: value for key, value in os.environ.items() if not key.startswith(('RPROXY_', 'COMPOSE_'))}
    with tempfile.TemporaryDirectory(prefix='rproxy-compose-') as directory:
        directory = Path(directory)
        config = directory / 'rproxy.toml'
        config.write_text((ROOT / 'rproxy.example.toml').read_text(encoding='utf-8').replace(
            'https://api.ipify.org/', 'http://fixture:18080/ip'), encoding='utf-8')
        collector = directory / 'proxy.toml'
        collector.write_text('workers=2\npoll_seconds=1\n[rproxy]\nurl="http://rproxy:9100"\ntoken_env="RPROXY_WORKER_TOKEN"\ntimeout_seconds=5\n', encoding='utf-8')
        providers = directory / 'providers.env'
        providers.write_text('# No external provider secrets.\n', encoding='utf-8')
        env_file = directory / '.env'
        env_file.write_text(f'RPROXY_PASSWORD={password}\nRPROXY_ADMIN_TOKEN={token}\nRPROXY_WORKER_TOKEN={worker_token}\n', encoding='utf-8')
        environment.update({
            'RPROXY_CONFIG': config.as_posix(), 'RPROXY_COLLECTOR_CONFIG': collector.as_posix(),
            'RPROXY_PROVIDERS_ENV': providers.as_posix(), 'RPROXY_BIND_ADDRESS': '127.0.0.1',
            'RPROXY_HTTP_PORT': str(http_port), 'RPROXY_SOCKS5_PORT': str(socks_port),
        })
        override = directory / 'smoke.json'
        override.write_text(json.dumps({'services': {
            'rproxy': {'image': f'{prefix}-rproxy'}, 'collector': {'image': f'{prefix}-collector'},
            'fixture': {'image': f'{prefix}-collector', 'entrypoint': ['python', '-I', '/fixture.py'], 'command': [],
                        'read_only': True, 'cap_drop': ['ALL'], 'volumes': [{'type': 'bind',
                        'source': (ROOT / 'scripts/deployment_tests/fixture.py').as_posix(), 'target': '/fixture.py',
                        'read_only': True, 'bind': {'create_host_path': False, 'selinux': 'Z'}}]},
        }}), encoding='utf-8')
        command = [args.engine, 'compose', '-p', project, '--env-file', str(env_file), '-f', str(ROOT / 'compose.yaml'), '-f', str(override)]

        def compose(*arguments, check=True, env=None, input=None):
            result = subprocess.run(command + list(arguments), cwd=ROOT, env=env or environment,
                                    input=input, text=True, encoding='utf-8', errors='replace', capture_output=True)
            if check and result.returncode:
                raise RuntimeError(f'Compose {arguments[0]} failed:\n{result.stdout}\n{result.stderr}')
            return result

        def cli(*arguments, input=None, check=True):
            result = compose('exec', '-T', 'rproxy', 'rproxy-cli', '--json', *arguments, input=input, check=check)
            return json.loads(result.stdout) if check else result

        def status():
            return cli('health')

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

        def source_manifest(revision=1):
            return f'''schema_version=1
[[sources]]
name="local-fixture"
kind="api"
enabled=true
[sources.config]
url="http://fixture:18080/supplier?revision={revision}"
protocol="http"
interval_seconds=3600
timeout_seconds=2
attempts=1
'''

        try:
            if not args.skip_build:
                print('Building deployment images and both Rust binaries', flush=True)
                compose('build')
            compose('config', '--quiet')
            for name in ('RPROXY_ADMIN_TOKEN', 'RPROXY_WORKER_TOKEN'):
                assert compose('config', '--quiet', check=False, env={**environment, name: ''}).returncode != 0
            missing = directory / 'missing.toml'
            result = compose('run', '--rm', '--no-deps', 'rproxy', 'check-config', check=False,
                             env={**environment, 'RPROXY_CONFIG': missing.as_posix()})
            assert result.returncode != 0 and not missing.exists()
            compose('up', '-d', '--no-build')
            wait_ready(lambda: status()['candidates'] == 0, 'empty managed pool')
            cli('source', 'import', '-', '--wait', '--wait-timeout', '60', input=source_manifest())
            wait_ready(lambda: status()['verified'] == 1, 'managed source acquisition and admission')
            username = 'v1|alice|shop|compose-smoke|stick'
            assert proxy_request(http_port, username, password) == (200, b'mounted-compose-ok')
            assert proxy_request(http_port, username, 'incorrect-password')[0] == 407
            socks_request(socks_port, username, password)
            original = cli('session', 'list', '--business', 'shop')['items'][0]
            cli('user', 'passwd', 'alice', '--password-stdin', input=new_password + '\n')
            assert proxy_request(http_port, username, password)[0] == 407
            assert proxy_request(http_port, username, new_password)[0] == 200
            rotated = cli('session', 'list', '--business', 'shop')['items'][0]
            assert (original['owner'], original['egress']) == (rotated['owner'], rotated['egress'])
            compose('stop', 'rproxy')
            compose('start', 'rproxy')
            wait_ready(lambda: status()['sessions'] == 1, 'durable managed restart')
            assert proxy_request(http_port, username, new_password)[0] == 200
            assert proxy_request(http_port, username, password)[0] == 407
            print('PASS managed acquisition, online password rotation, sticky egress and restart', flush=True)

            edited = config.read_text(encoding='utf-8').replace('name = "alice"', 'name = "file-only-user"').replace(
                'mode = "required"', 'mode = "anonymous"').replace('default_strategy = "random"', 'default_strategy = "round-robin"')
            replacement = directory / 'edited.toml'
            replacement.write_text(edited, encoding='utf-8')
            replacement.replace(config)
            compose('up', '-d', '--no-build', '--force-recreate', 'rproxy', 'collector')
            wait_ready(lambda: status()['verified'] == 1, 'host startup config edit')
            assert proxy_request(http_port)[0] == 200
            names = [user['name'] for user in cli('user', 'list')['items']]
            assert 'alice' in names and 'file-only-user' not in names
            assert proxy_request(http_port, username, new_password)[0] == 200
            replacement.write_text(edited.replace('mode = "anonymous"', 'mode = "required"'), encoding='utf-8')
            replacement.replace(config)
            compose('up', '-d', '--no-build', '--force-recreate', 'rproxy', 'collector')
            wait_ready(lambda: status()['verified'] == 1, 'required policy restored')
            assert proxy_request(http_port)[0] == 407
            print('PASS mounted startup policy; old TOML users never overwrite managed users', flush=True)

            cli('user', 'add', 'bob', '--business', 'billing', '--password-stdin', input=bob_password + '\n')
            bob = 'v1|bob|billing|created-online|stick'
            assert proxy_request(http_port, bob, bob_password)[0] == 200
            cli('user', 'disable', 'bob', '--yes')
            assert proxy_request(http_port, bob, bob_password)[0] == 407
            cli('user', 'enable', 'bob')
            assert proxy_request(http_port, bob, bob_password)[0] == 200
            cli('source', 'pause', 'local-fixture')
            assert cli('source', 'run', 'local-fixture', check=False).returncode != 0
            cli('source', 'resume', 'local-fixture')
            cli('source', 'run', 'local-fixture', '--wait', '--wait-timeout', '60')
            cli('source', 'import', '-', '--replace', '--wait', '--wait-timeout', '60', input=source_manifest(2))
            address = cli('proxy', 'list', '--protocol', 'http')['items'][0]['address']
            host = address.rsplit(':', 1)[0]
            cli('proxy', 'add', 'http', address, '--source', 'manual-http', '--wait')
            cli('proxy', 'add', 'socks5', host + ':18081', '--source', 'manual-socks', '--wait', '--wait-timeout', '60')
            cli('source', 'remove', 'local-fixture', '--yes')
            assert cli('proxy', 'show', 'http:' + address)['state'] == 'ready'
            assert cli('proxy', 'disable', 'http:' + address, '--dry-run')['applied'] is False
            cli('proxy', 'disable', 'http:' + address, '--yes')
            assert proxy_request(http_port, 'v1|alice|news|manual-socks|stick', new_password)[0] == 200
            socks_request(socks_port, username, new_password)
            print('PASS online users/sources, manual HTTP/SOCKS5 and non-destructive source removal', flush=True)

            with open_tunnel(http_port, username, new_password) as stream:
                cli('user', 'disable', 'alice', '--disconnect', '--yes')
                stream.settimeout(3)
                try:
                    assert stream.recv(1) == b'', 'Force-disconnected tunnel stayed alive'
                except (ConnectionResetError, ConnectionAbortedError):
                    pass
            cli('user', 'enable', 'alice')
            assert proxy_request(http_port, username, new_password)[0] == 200
            cli('user', 'remove', 'alice', '--disconnect', '--yes')
            cli('user', 'remove', 'bob', '--disconnect', '--yes')
            assert cli('user', 'list')['items'] == []
            assert proxy_request(http_port)[0] == 407
            compose('stop', 'rproxy')
            compose('start', 'rproxy')
            wait_ready(lambda: bool(status()), 'empty required-auth pool restart')
            assert cli('user', 'list')['items'] == []
            assert proxy_request(http_port)[0] == 407
            cli('session', 'release', 'shop', 'compose-smoke', '--yes')
            worker_probe = '''import os,urllib.request,urllib.error
assert 'RPROXY_ADMIN_TOKEN' not in os.environ
r=urllib.request.Request('http://rproxy:9100/v1/users',headers={'Authorization':'Bearer '+os.environ['RPROXY_WORKER_TOKEN']})
try: urllib.request.urlopen(r,timeout=5); raise AssertionError('worker obtained root access')
except urllib.error.HTTPError as e: assert e.code==403
'''
            compose('exec', '-T', 'collector', 'python', '-I', '-c', worker_probe)
            collector_id = compose('ps', '-q', 'collector').stdout.strip()
            running = subprocess.run([args.engine, 'inspect', '--format', '{{.State.Running}}', collector_id],
                                     capture_output=True, text=True, check=True).stdout.strip()
            assert running == 'true'
            print('PASS force disconnect, last-user fail-closed, admin recovery and scoped idle worker', flush=True)
        except Exception:
            logs = compose('logs', '--no-color', '--tail', '100', check=False)
            text = logs.stdout + logs.stderr
            for secret in (password, new_password, bob_password, token, worker_token):
                text = text.replace(secret, '<redacted>')
            print(text, flush=True)
            raise
        finally:
            result = compose('down', '--volumes', '--remove-orphans', check=False)
            if result.returncode:
                raise RuntimeError(f'Could not clean up owned Compose project {project}: {result.stderr}')


if __name__ == '__main__':
    main()
