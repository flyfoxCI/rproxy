import json
import os
from pathlib import Path
import sys
import tempfile
import threading
import unittest
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from unittest.mock import patch

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
import collector


class CollectorTests(unittest.TestCase):
    def test_text_and_json_mapping_normalize_and_deduplicate(self):
        self.assertEqual(collector.addresses('1.2.3.4:80\n1.2.3.4:80,[::1]:1080', {'format': 'text'}), ['1.2.3.4:80', '[::1]:1080'])
        body = json.dumps({'code': 0, 'data': {'items': [{'host': '1.2.3.4', 'port': 80}]}})
        mapping = {'format': 'json', 'items_path': 'data.items', 'ip_path': 'host', 'port_path': 'port', 'success_path': 'code', 'success_value': 0}
        self.assertEqual(collector.addresses(body, mapping), ['1.2.3.4:80'])
        mapping = {'format': 'json', 'items_path': '', 'address_path': ''}
        self.assertEqual(collector.addresses('["1.2.3.4:80"]', mapping), ['1.2.3.4:80'])

    def test_bad_response_and_credentials_in_addresses_are_rejected(self):
        for value in ['http://user:password@1.2.3.4:80', '1.2.3.4:0', 'supplier.example:80', '0.0.0.0:80', 'not a proxy']:
            with self.assertRaises(ValueError):
                collector.addresses(value, {'format': 'text'})
        with self.assertRaises(ValueError):
            collector.addresses('{"code":1,"data":[]}', {'format': 'json', 'items_path': 'data', 'success_path': 'code', 'success_value': 0})

    def test_environment_expansion_requires_values(self):
        with patch.dict(os.environ, {'TEST_PROVIDER_SECRET': 'private'}, clear=False):
            self.assertEqual(collector.expand({'headers': {'X-Key': '${TEST_PROVIDER_SECRET}'}}), {'headers': {'X-Key': 'private'}})
        with patch.dict(os.environ, {}, clear=True):
            with self.assertRaises(ValueError):
                collector.expand('${MISSING_PROVIDER_SECRET}')

    def test_strict_toml_and_disabled_providers(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / 'proxy.toml'
            path.write_text('[[providers]]\nname="unused"\nenabled=false\nurl="${MISSING_SECRET}"\n', encoding='utf-8')
            config = collector.load_config(path)
            self.assertEqual(config['providers'], [])
            path.write_text('typo=true', encoding='utf-8')
            with self.assertRaises(ValueError):
                collector.load_config(path)
            path.write_text('[[providers]]\nname="test"\nurl="https://supplier.example"\nprotocol="socks4"', encoding='utf-8')
            with self.assertRaises(ValueError):
                collector.load_config(path)

    def test_missing_secrets_are_actionable_without_values(self):
        import subprocess
        with tempfile.TemporaryDirectory() as directory, patch.dict(os.environ, {'REQUIRED_SUPPLIER_URL': '', 'REQUIRED_ADMIN_TOKEN': ''}):
            path = Path(directory) / 'proxy.toml'
            path.write_text('[[providers]]\nname="test"\nurl="${REQUIRED_SUPPLIER_URL}"\n', encoding='utf-8')
            result = subprocess.run([sys.executable, str(Path(collector.__file__)), '--config', str(path), '--check-config'],
                                    capture_output=True, text=True, timeout=10)
            self.assertNotEqual(result.returncode, 0)
            self.assertIn('REQUIRED_SUPPLIER_URL', result.stderr)
            path.write_text('[rproxy]\ntoken_env="REQUIRED_ADMIN_TOKEN"\n[[providers]]\nname="test"\nurl="https://supplier.example"\n', encoding='utf-8')
            with self.assertRaisesRegex(ValueError, 'REQUIRED_ADMIN_TOKEN'):
                collector.load_config(path)

    def test_local_get_post_and_management_submission(self):
        received = []

        class Handler(BaseHTTPRequestHandler):
            def do_GET(self):
                received.append(('GET', self.path, self.headers.get('X-Test')))
                self.send_response(200)
                self.end_headers()
                self.wfile.write(b'1.2.3.4:80\n')

            def do_POST(self):
                body = json.loads(self.rfile.read(int(self.headers['Content-Length'])))
                received.append(('POST', self.path, body, self.headers.get('Authorization')))
                self.send_response(202 if self.path == '/v1/proxies' else 200)
                self.end_headers()
                self.wfile.write(b'{"accepted":1}' if self.path == '/v1/proxies' else b'{"list":["2.3.4.5:81"]}')

            def log_message(self, *args):
                pass

        server = ThreadingHTTPServer(('127.0.0.1', 0), Handler)
        thread = threading.Thread(target=server.serve_forever, daemon=True)
        thread.start()
        try:
            with tempfile.TemporaryDirectory() as directory, patch.dict(os.environ, {'TEST_ADMIN_TOKEN': 'a-test-token-with-16-characters'}):
                url = f'http://127.0.0.1:{server.server_port}'
                path = Path(directory) / 'proxy.toml'
                path.write_text(f'''[rproxy]
url="{url}"
token_env="TEST_ADMIN_TOKEN"
[[providers]]
name="get-source"
url="{url}/extract"
protocol="http"
[providers.params]
count=2
[providers.headers]
X-Test="present"
[[providers]]
name="post-source"
url="{url}/extract"
method="POST"
protocol="socks5"
[providers.json]
count=1
[providers.response]
format="json"
items_path="list"
''', encoding='utf-8')
                config = collector.load_config(path)
                self.assertEqual(collector.run_once(config), 2)
                imports = [r for r in received if r[1] == '/v1/proxies']
                self.assertEqual(len(imports), 2)
                self.assertEqual(imports[0][2]['addresses'], ['1.2.3.4:80'])
                self.assertEqual(imports[1][2]['protocol'], 'socks5')
                self.assertEqual(imports[0][3], 'Bearer a-test-token-with-16-characters')
                self.assertIn(('GET', '/extract?count=2', 'present'), received)
        finally:
            server.shutdown()
            server.server_close()
            thread.join()

    def test_fetch_retries_are_bounded_and_do_not_leak_url(self):
        class Handler(BaseHTTPRequestHandler):
            calls = 0
            def do_GET(self):
                type(self).calls += 1
                self.send_response(503)
                self.end_headers()
            def log_message(self, *args):
                pass
        server = ThreadingHTTPServer(('127.0.0.1', 0), Handler)
        thread = threading.Thread(target=server.serve_forever, daemon=True)
        thread.start()
        try:
            provider = {'name': 'test', 'url': f'http://127.0.0.1:{server.server_port}/?key=secret', 'method': 'GET', 'attempts': 2, 'timeout_seconds': 1, 'response': {'format': 'text'}}
            with self.assertRaises(collector.CollectionError) as failure:
                collector.fetch(provider)
            self.assertNotIn('secret', str(failure.exception))
            self.assertEqual(Handler.calls, 2)
        finally:
            server.shutdown()
            server.server_close()
            thread.join()


if __name__ == '__main__':
    unittest.main()
