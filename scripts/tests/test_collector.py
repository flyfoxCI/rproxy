from contextlib import contextmanager
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import threading
import time
import unittest
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from unittest.mock import patch

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
import collector


TOKEN = 'local-test-worker-token'


class FakeAPI:
    """Local supplier plus least-privileged worker API; never contacts a provider."""

    def __init__(self):
        self.sources = []
        self.requests = []
        self.results = []
        self.plan_status = 200
        self.result_status = 202
        self.plan_body = None
        self.result_body = None
        self.routes = {'/extract': (200, b'1.2.3.4:80\n')}
        self.fetch_hook = None
        api = self

        class Handler(BaseHTTPRequestHandler):
            def handle_request(self):
                body = self.rfile.read(int(self.headers.get('Content-Length', 0)))
                api.requests.append((self.command, self.path, dict(self.headers), body))
                if self.path.startswith('/v1/'):
                    if self.headers.get('Authorization') != 'Bearer ' + TOKEN:
                        status, content = 401, b'{"error":"denied"}'
                    elif self.command == 'GET' and self.path == '/v1/collector/plan':
                        status = api.plan_status
                        content = api.plan_body if api.plan_body is not None else json.dumps(
                            {'version': '0.2.3', 'sources': api.sources}).encode()
                    elif self.command == 'POST' and self.path == '/v1/collector/results':
                        result = json.loads(body)
                        api.results.append(result)
                        status = api.result_status
                        content = api.result_body if api.result_body is not None else json.dumps(
                            {'accepted': len(result['addresses'])}).encode()
                    else:
                        status, content = 403, b'{"error":"worker role only"}'
                else:
                    if api.fetch_hook:
                        api.fetch_hook(self.path)
                    status, content = api.routes.get(self.path.split('?')[0], (404, b''))
                self.send_response(status)
                length = sum(len(chunk) for chunk, _ in content) if isinstance(content, list) else len(content)
                self.send_header('Content-Length', str(length))
                self.end_headers()
                try:
                    if isinstance(content, list):
                        for chunk, delay in content:
                            time.sleep(delay)
                            self.wfile.write(chunk)
                            self.wfile.flush()
                    else:
                        self.wfile.write(content)
                except (BrokenPipeError, ConnectionResetError, ConnectionAbortedError):
                    pass

            do_GET = do_POST = handle_request

            def log_message(self, *args):
                pass

        self.server = ThreadingHTTPServer(('127.0.0.1', 0), Handler)
        self.server.daemon_threads = True
        self.thread = threading.Thread(target=lambda: self.server.serve_forever(poll_interval=0.01), daemon=True)

    def __enter__(self):
        self.thread.start()
        self.url = f'http://127.0.0.1:{self.server.server_port}'
        return self

    def __exit__(self, *args):
        self.server.shutdown()
        self.server.server_close()
        self.thread.join(timeout=3)

    def source(self, identity='s_first', version=1, generation=0, **fields):
        return {'id': identity, 'name': 'local-source', 'version': version, 'run_generation': generation,
                'config': {'name': 'local-source', 'url': self.url + '/extract', 'protocol': 'http',
                           'interval_seconds': 60, 'attempts': 1, **fields}}

    def fetches(self):
        return [request for request in self.requests if not request[1].startswith('/v1/')]


@contextmanager
def bootstrap(api=None, text=''):
    with tempfile.TemporaryDirectory() as directory:
        path = Path(directory) / 'proxy.toml'
        path.write_text((f'[rproxy]\nurl="{api.url}"\n' if api else '') + text, encoding='utf-8')
        yield path


def finish(worker, now=0):
    """Reap the current bounded jobs while leaving the clock under test control."""
    for _ in range(500):
        worker.tick(now)
        if not worker.pending:
            return
        time.sleep(0.005)
    raise AssertionError('local worker job did not finish')


class CollectorTests(unittest.TestCase):
    def test_text_and_json_mapping_normalize_and_deduplicate(self):
        self.assertEqual(collector.addresses('1.2.3.4:80\n1.2.3.4:80,[::1]:1080', {'format': 'text'}),
                         ['1.2.3.4:80', '[::1]:1080'])
        body = json.dumps({'code': 0, 'data': {'items': [{'host': '1.2.3.4', 'port': 80}]}})
        mapping = {'format': 'json', 'items_path': 'data.items', 'ip_path': 'host', 'port_path': 'port',
                   'success_path': 'code', 'success_value': 0}
        self.assertEqual(collector.addresses(body, mapping), ['1.2.3.4:80'])
        self.assertEqual(collector.addresses('["1.2.3.4:80"]', {'format': 'json'}), ['1.2.3.4:80'])
        self.assertEqual(collector.addresses('{"a":[{"endpoint":"[::1]:80"}]}',
                         {'format': 'json', 'items_path': 'a', 'address_path': 'endpoint'}), ['[::1]:80'])

    def test_bad_response_and_credentials_in_addresses_are_rejected(self):
        for value in ['http://user:password@1.2.3.4:80', '1.2.3.4:0', 'supplier.example:80',
                      '0.0.0.0:80', '[::]:80', '[fe80::1%eth0]:80', 'not a proxy']:
            with self.subTest(value=value), self.assertRaises(ValueError):
                collector.addresses(value, {'format': 'text'})
        with self.assertRaises(ValueError):
            collector.addresses('{"code":1,"data":[]}',
                                {'format': 'json', 'items_path': 'data', 'success_path': 'code', 'success_value': 0})
        with self.assertRaises(ValueError):
            collector.addresses('1.2.3.4:80\n' * (collector.MAX_ADDRESSES + 1), {'format': 'text'})

    def test_environment_expansion_requires_values(self):
        with patch.dict(os.environ, {'TEST_PROVIDER_SECRET': 'private'}):
            self.assertEqual(collector.expand({'headers': {'X-Key': '${TEST_PROVIDER_SECRET}'}}),
                             {'headers': {'X-Key': 'private'}})
        with patch.dict(os.environ, {}, clear=True), self.assertRaises(ValueError):
            collector.expand('${MISSING_PROVIDER_SECRET}')

    def test_bootstrap_defaults_need_no_token_and_do_not_include_sources(self):
        with bootstrap() as path, patch.dict(os.environ, {'RPROXY_WORKER_TOKEN': '', 'RPROXY_ADMIN_TOKEN': TOKEN}):
            config = collector.load_config(path)
            self.assertEqual(config['rproxy']['token_env'], 'RPROXY_WORKER_TOKEN')
            self.assertEqual(config['poll_seconds'], 3)
            self.assertNotIn('providers', config)
            self.assertEqual(config['config_dir'], path.parent.resolve())
            result = self.cli(path, '--check-config')
            self.assertEqual(result.returncode, 0, result.stderr)

    def test_nonempty_legacy_providers_require_explicit_import_even_when_disabled(self):
        with bootstrap(text='[[providers]]\nname="old"\nenabled=false\nurl="${SECRET}"\n') as path:
            with self.assertRaisesRegex(ValueError, 'rproxy-cli source import'):
                collector.load_config(path)
        with bootstrap(text='providers=[]\n') as path:
            self.assertNotIn('providers', collector.load_config(path))

    def test_invalid_bootstrap_is_rejected_with_value_free_diagnostics(self):
        invalid = ['typo="SECRET"', 'poll_seconds=0', 'workers=33', 'poll_seconds=true',
                   '[rproxy]\nurl="file:///SECRET"', '[rproxy]\nurl="http://[SECRET"',
                   '[rproxy]\nurl="http://127.0.0.1/SECRET"', '[rproxy]\nurl="http://user:SECRET@localhost"',
                   '[rproxy]\ntoken_env=42', '[rproxy]\nca_certificate=42', 'workers="SECRET"',
                   '[[providers]]\nname="unterminated-SECRET']
        for text in invalid:
            with self.subTest(text=text), bootstrap(text=text) as path:
                with self.assertRaises(collector.ConfigurationError) as failure:
                    collector.load_config(path)
                self.assertNotIn('SECRET', str(failure.exception))

    def test_live_modes_require_worker_token_even_with_no_sources(self):
        with FakeAPI() as api, bootstrap(api) as path, patch.dict(os.environ, {'RPROXY_WORKER_TOKEN': '', 'RPROXY_ADMIN_TOKEN': TOKEN}):
            for options in [('--once',), ()]:
                result = self.cli(path, *options)
                self.assertNotEqual(result.returncode, 0)
                self.assertIn('worker token', result.stderr.lower())
            self.assertEqual(api.requests, [])

    def test_once_get_post_and_exact_worker_result_contract(self):
        with FakeAPI() as api, bootstrap(api) as path, patch.dict(os.environ, {'RPROXY_WORKER_TOKEN': TOKEN}):
            api.sources = [api.source(params={'count': 2}, headers={'X-Test': '${LOCAL_HEADER}'}),
                           api.source('s_second', version=4, generation=7, method='POST', protocol='socks5',
                                      json={'count': 1}, response={'format': 'json', 'items_path': 'list'})]
            api.sources[1]['config']['url'] = api.url + '/post'
            api.routes['/post'] = (200, b'{"list":["2.3.4.5:81"]}')
            with patch.dict(os.environ, {'LOCAL_HEADER': 'present'}):
                self.assertEqual(collector.run_once(collector.load_config(path)), 2)
            self.assertEqual(len(api.results), 2)
            results = {item['source_id']: item for item in api.results}
            self.assertEqual(results['s_first'], {'source_id': 's_first', 'version': 1, 'run_generation': 0,
                             'addresses': ['1.2.3.4:80'], 'complete': True, 'error': None})
            self.assertEqual(results['s_second']['version'], 4)
            self.assertEqual(results['s_second']['run_generation'], 7)
            self.assertEqual(results['s_second']['addresses'], ['2.3.4.5:81'])
            management = [r for r in api.requests if r[1].startswith('/v1/')]
            self.assertEqual({r[1] for r in management}, {'/v1/collector/plan', '/v1/collector/results'})
            self.assertTrue(all(r[2]['Authorization'] == 'Bearer ' + TOKEN for r in management))
            get = next(r for r in api.fetches() if r[0] == 'GET')
            post = next(r for r in api.fetches() if r[0] == 'POST')
            self.assertEqual(get[1], '/extract?count=2')
            self.assertEqual(get[2]['X-Test'], 'present')
            self.assertNotIn('Authorization', get[2])
            self.assertEqual(json.loads(post[3]), {'count': 1})

    def test_empty_success_still_finishes_generation(self):
        with FakeAPI() as api, bootstrap(api) as path, patch.dict(os.environ, {'RPROXY_WORKER_TOKEN': TOKEN}):
            api.sources = [api.source()]
            api.routes['/extract'] = (200, b'')
            self.assertEqual(collector.run_once(collector.load_config(path)), 0)
            self.assertEqual(api.results[0]['addresses'], [])
            self.assertTrue(api.results[0]['complete'])
            self.assertIsNone(api.results[0]['error'])

    def test_batches_are_at_most_256_and_only_last_is_complete(self):
        with FakeAPI() as api, bootstrap(api) as path, patch.dict(os.environ, {'RPROXY_WORKER_TOKEN': TOKEN}):
            api.sources = [api.source()]
            api.routes['/extract'] = (200, '\n'.join(f'1.2.3.4:{port}' for port in range(1, 514)).encode())
            self.assertEqual(collector.run_once(collector.load_config(path)), 513)
            self.assertEqual([len(r['addresses']) for r in api.results], [256, 256, 1])
            self.assertEqual([r['complete'] for r in api.results], [False, False, True])
            self.assertTrue(all(set(r) == {'source_id', 'version', 'run_generation', 'addresses', 'complete', 'error'}
                                for r in api.results))

    def test_dynamic_add_edit_pause_resume_remove_and_manual_generation(self):
        with FakeAPI() as api, bootstrap(api) as path, patch.dict(os.environ, {'RPROXY_WORKER_TOKEN': TOKEN}):
            with collector.Worker(collector.load_config(path)) as worker:
                worker.tick(0)
                self.assertEqual(len(api.fetches()), 0)
                api.sources = [api.source()]
                finish(worker, 3)
                self.assertEqual(len(api.fetches()), 1)
                worker.tick(5)
                self.assertEqual(len(api.fetches()), 1)
                api.sources[0]['version'] = 2
                api.sources[0]['config']['url'] = api.url + '/edited'
                api.routes['/edited'] = (200, b'2.3.4.5:81')
                finish(worker, 6)
                self.assertEqual(api.results[-1]['addresses'], ['2.3.4.5:81'])
                api.sources = []  # paused sources are absent from the plan
                finish(worker, 9)
                finish(worker, 90)
                self.assertEqual(len(api.fetches()), 2)
                api.sources = [api.source(version=4)]
                finish(worker, 93)
                self.assertEqual(len(api.fetches()), 3)
                api.sources[0]['run_generation'] = 1
                finish(worker, 96)
                self.assertEqual(api.results[-1]['run_generation'], 1)
                self.assertEqual(len(api.fetches()), 4)
                finish(worker, 155)
                self.assertEqual(len(api.fetches()), 4)
                finish(worker, 156)
                self.assertEqual(len(api.fetches()), 5)
                api.sources = []  # deletion also removes its schedule
                finish(worker, 159)
                finish(worker, 300)
                self.assertEqual(len(api.fetches()), 5)

    def test_running_old_revision_is_not_duplicated_or_allowed_to_delay_new_revision(self):
        started, release = threading.Event(), threading.Event()
        with FakeAPI() as api, bootstrap(api) as path, patch.dict(os.environ, {'RPROXY_WORKER_TOKEN': TOKEN}):
            def block_first(_):
                if not started.is_set():
                    started.set()
                    release.wait(3)
            api.fetch_hook = block_first
            api.sources = [api.source()]
            with collector.Worker(collector.load_config(path)) as worker:
                try:
                    worker.tick(0)
                    self.assertTrue(started.wait(2))
                    api.sources[0]['version'] = 2
                    api.sources[0]['run_generation'] = 3
                    worker.tick(3)
                    self.assertEqual(len(api.fetches()), 1)
                    release.set()
                    finish(worker, 3)
                    self.assertEqual([r['version'] for r in api.results], [1, 2])
                    self.assertEqual(api.results[-1]['run_generation'], 3)
                finally:
                    release.set()

    def test_409_stops_batches_and_does_not_retry_stale_generation(self):
        with FakeAPI() as api, bootstrap(api) as path, patch.dict(os.environ, {'RPROXY_WORKER_TOKEN': TOKEN}):
            api.sources = [api.source()]
            api.result_status = 409
            api.routes['/extract'] = (200, '\n'.join(f'1.2.3.4:{p}' for p in range(1, 514)).encode())
            with collector.Worker(collector.load_config(path)) as worker:
                finish(worker, 0)
                self.assertEqual(len(api.results), 1)
                finish(worker, 3)
                finish(worker, 300)
                self.assertEqual(len(api.results), 1)
                api.result_status = 202
                api.sources[0]['run_generation'] = 1
                finish(worker, 303)
                self.assertEqual(len(api.results), 4)
                self.assertEqual(api.results[-1]['run_generation'], 1)

    def test_worker_cannot_use_root_routes_and_worker_rejects_forbidden_plan_without_retry(self):
        with FakeAPI() as api, bootstrap(api) as path, patch.dict(os.environ, {'RPROXY_WORKER_TOKEN': TOKEN}):
            with collector.session() as client:
                response = client.get(api.url + '/v1/sources', headers={'Authorization': 'Bearer ' + TOKEN}, timeout=2)
                self.assertEqual(response.status_code, 403)
                response.close()
            api.plan_status = 403
            result = self.cli(path, '--once')
            self.assertNotEqual(result.returncode, 0)
            self.assertIn('authorization', result.stderr.lower())
            self.assertEqual(sum(r[1] == '/v1/collector/plan' for r in api.requests), 1)
            self.assertEqual(api.results, [])

    def test_unavailable_plan_retries_on_poll_but_idle_loop_stays_alive(self):
        with FakeAPI() as api, bootstrap(api) as path, patch.dict(os.environ, {'RPROXY_WORKER_TOKEN': TOKEN}):
            api.plan_status = 503
            with collector.Worker(collector.load_config(path)) as worker:
                with self.assertLogs(level='WARNING'):
                    worker.tick(0)
                worker.tick(1)
                self.assertEqual(len(api.requests), 1)
                api.plan_status = 200
                worker.tick(3)
                self.assertEqual(len(api.requests), 2)
                api.sources = [api.source()]
                finish(worker, 6)
                self.assertEqual(len(api.results), 1)
            api.sources = []
            stop = threading.Event()
            thread = threading.Thread(target=collector.run, args=(collector.load_config(path), stop), daemon=True)
            thread.start()
            try:
                time.sleep(0.05)
                self.assertTrue(thread.is_alive())
            finally:
                stop.set()
                thread.join(3)
            self.assertFalse(thread.is_alive())

    def test_invalid_plans_are_bounded_and_do_not_replace_previous_schedule(self):
        with FakeAPI() as api, bootstrap(api) as path, patch.dict(os.environ, {'RPROXY_WORKER_TOKEN': TOKEN}):
            api.sources = [api.source()]
            with collector.Worker(collector.load_config(path)) as worker:
                finish(worker, 0)
                for now, payload in enumerate([{'version': 'other', 'sources': []},
                        {'version': '0.2.3', 'sources': [api.source(), api.source()]},
                        {'version': '0.2.3', 'sources': [api.source(version=True)]},
                        {'version': '0.2.3', 'sources': 'SECRET'}, {'version': '0.2.3', 'sources': [{}]}], 1):
                    api.plan_body = json.dumps(payload).encode()
                    with self.assertLogs(level='WARNING'):
                        worker.tick(now * 3)
                    self.assertEqual(len(api.fetches()), 1)
                api.plan_body = None
                finish(worker, 60)
                self.assertEqual(len(api.results), 2)

    def test_source_env_mapping_and_fetch_errors_are_safe_results_and_once_fails(self):
        secret = 'DO-NOT-LOG-provider-secret'
        with FakeAPI() as api, bootstrap(api) as path, patch.dict(os.environ, {'RPROXY_WORKER_TOKEN': TOKEN}):
            api.routes['/bad'] = (200, secret.encode())
            api.routes['/failed'] = (503, secret.encode())
            api.sources = [api.source('s_env', headers={'X-Key': '${MISSING_LOCAL_KEY}'}),
                           api.source('s_mapping', url=api.url + '/bad?token=' + secret),
                           api.source('s_fetch', url=api.url + '/failed?token=' + secret, attempts=2),
                           api.source('s_valid')]
            with patch.dict(os.environ, {'MISSING_LOCAL_KEY': ''}):
                result = self.cli(path, '--once')
            self.assertNotEqual(result.returncode, 0)
            errors = {r['source_id']: r['error'] for r in api.results}
            self.assertEqual(errors, {'s_env': 'env-missing', 's_mapping': 'mapping-failed',
                                      's_fetch': 'fetch-failed', 's_valid': None})
            self.assertEqual(len([r for r in api.fetches() if r[1].startswith('/failed')]), 2)
            self.assertEqual(len([r for r in api.fetches() if r[1].startswith('/bad')]), 1)
            for value in [secret, TOKEN, 'MISSING_LOCAL_KEY', api.url]:
                self.assertNotIn(value, result.stdout + result.stderr)
            self.assertTrue(all(r['addresses'] == [] and r['complete'] for r in api.results if r['error']))

    def test_plan_source_ca_paths_resolve_relative_to_bootstrap(self):
        with FakeAPI() as api, bootstrap(api, 'ca_certificate="certs/root.pem"\n') as path:
            config = collector.load_config(path)
            self.assertEqual(config['rproxy']['ca_certificate'], str((path.parent / 'certs/root.pem').resolve()))
            provider = collector.prepare_provider(api.source(ca_certificate='certs/supplier.pem')['config'], path.parent)
            self.assertEqual(provider['ca_certificate'], str((path.parent / 'certs/supplier.pem').resolve()))

    def test_invalid_received_config_reports_mapping_error_not_raw_values(self):
        with FakeAPI() as api, bootstrap(api) as path, patch.dict(os.environ, {'RPROXY_WORKER_TOKEN': TOKEN}):
            for fields in [{'url': 'http://[SECRET'}, {'method': 'DELETE'}, {'attempts': 99},
                           {'response': {'format': 'python', 'code': 'SECRET'}}, {'headers': []},
                           {'json': {'SECRET': 1}}, {'insecure': 'SECRET'}, {'ca_certificate': 42}]:
                api.sources = [api.source(**fields)]
                with self.assertLogs(level='WARNING'), self.assertRaises(collector.CollectionError):
                    collector.run_once(collector.load_config(path))
                self.assertEqual(api.results[-1]['error'], 'mapping-failed')
            self.assertEqual(api.fetches(), [])

    def test_response_bounds_for_provider_plan_and_acknowledgement(self):
        with FakeAPI() as api, bootstrap(api) as path, patch.dict(os.environ, {'RPROXY_WORKER_TOKEN': TOKEN}):
            api.sources = [api.source()]
            api.routes['/extract'] = (200, b'x' * (collector.MAX_RESPONSE + 1))
            with self.assertLogs(level='WARNING'), self.assertRaises(collector.CollectionError):
                collector.run_once(collector.load_config(path))
            self.assertEqual(api.results[-1]['error'], 'fetch-failed')
            api.plan_body = b'x' * (collector.MAX_RESPONSE + 1)
            with self.assertRaises(collector.CollectionError):
                collector.run_once(collector.load_config(path))
            api.plan_body = None
            api.routes['/extract'] = (200, b'1.2.3.4:80')
            api.result_body = b'x' * (collector.MAX_ACK_RESPONSE + 1)
            with self.assertLogs(level='WARNING'), self.assertRaises(collector.CollectionError):
                collector.run_once(collector.load_config(path))

    def test_redirects_and_proxy_environment_are_not_inherited(self):
        with collector.session() as client:
            self.assertFalse(client.trust_env)
        with FakeAPI() as api, bootstrap(api) as path, patch.dict(os.environ, {'RPROXY_WORKER_TOKEN': TOKEN,
                                                                 'HTTP_PROXY': 'http://127.0.0.1:1'}):
            api.sources = [api.source()]
            api.routes['/extract'] = (302, b'http://SECRET')
            with self.assertLogs(level='WARNING'), self.assertRaises(collector.CollectionError):
                collector.run_once(collector.load_config(path))
            self.assertEqual(api.results[-1]['error'], 'fetch-failed')
            api.plan_status = 302
            with self.assertRaises(collector.CollectionError):
                collector.run_once(collector.load_config(path))

    def test_once_idle_success_and_failures_have_useful_exit_codes(self):
        with FakeAPI() as api, bootstrap(api) as path, patch.dict(os.environ, {'RPROXY_WORKER_TOKEN': TOKEN}):
            result = self.cli(path, '--once')
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertIn('Accepted 0', result.stdout)
            api.plan_status = 503
            result = self.cli(path, '--once')
            self.assertNotEqual(result.returncode, 0)
            self.assertEqual(len(api.requests), 2)

    def test_slow_drip_response_does_not_escape_source_timeout(self):
        with FakeAPI() as api, bootstrap(api) as path, patch.dict(os.environ, {'RPROXY_WORKER_TOKEN': TOKEN}):
            api.sources = [api.source(timeout_seconds=1)]
            api.routes['/extract'] = (200, [(bytes([byte]), 0.3) for byte in b'1.2.3.4:80'])
            started = time.monotonic()
            with self.assertLogs(level='WARNING'), self.assertRaises(collector.CollectionError):
                collector.run_once(collector.load_config(path))
            self.assertLess(time.monotonic() - started, 2.5)
            self.assertEqual(api.results[-1]['error'], 'fetch-failed')

    def test_worker_limit_does_not_queue_sources_removed_before_capacity_is_free(self):
        started, release = threading.Event(), threading.Event()
        with FakeAPI() as api, bootstrap(api) as path, patch.dict(os.environ, {'RPROXY_WORKER_TOKEN': TOKEN}):
            def block(_):
                started.set()
                release.wait(3)
            api.fetch_hook = block
            api.sources = [api.source('s_first'), api.source('s_removed'), api.source('s_survives')]
            config = collector.load_config(path)
            config['workers'] = 1
            with collector.Worker(config) as worker:
                try:
                    worker.tick(0)
                    self.assertTrue(started.wait(2))
                    self.assertEqual(len(worker.pending), 1)
                    api.sources = [api.sources[0], api.sources[2]]
                    worker.tick(3)
                    release.set()
                    finish(worker, 3)
                    self.assertEqual({r['source_id'] for r in api.results}, {'s_first', 's_survives'})
                    self.assertEqual(len(api.fetches()), 2)
                finally:
                    release.set()

    def test_paused_inflight_source_finishes_stale_without_rescheduling(self):
        started, release = threading.Event(), threading.Event()
        with FakeAPI() as api, bootstrap(api) as path, patch.dict(os.environ, {'RPROXY_WORKER_TOKEN': TOKEN}):
            def block(_):
                started.set()
                release.wait(3)
            api.fetch_hook = block
            api.sources = [api.source()]
            with collector.Worker(collector.load_config(path)) as worker:
                try:
                    worker.tick(0)
                    self.assertTrue(started.wait(2))
                    api.sources = []
                    api.result_status = 409
                    worker.tick(3)
                    release.set()
                    finish(worker, 3)
                    finish(worker, 300)
                    self.assertEqual(len(api.fetches()), 1)
                    self.assertEqual(len(api.results), 1)
                finally:
                    release.set()

    def test_forbidden_results_and_invalid_acknowledgements_stop_batches(self):
        with FakeAPI() as api, bootstrap(api) as path, patch.dict(os.environ, {'RPROXY_WORKER_TOKEN': TOKEN}):
            api.sources = [api.source()]
            api.routes['/extract'] = (200, '\n'.join(f'1.2.3.4:{p}' for p in range(1, 258)).encode())
            api.result_status = 403
            with self.assertRaises(collector.WorkerAccessError):
                collector.run_once(collector.load_config(path))
            self.assertEqual(len(api.results), 1)
            api.result_status = 202
            for body in [b'{"accepted":true}', b'{"accepted":999}', b'{"accepted":-1}', b'"SECRET"', b'not-JSON-SECRET']:
                api.result_body = body
                before = len(api.results)
                with self.assertLogs(level='WARNING'), self.assertRaises(collector.CollectionError) as failure:
                    collector.run_once(collector.load_config(path))
                self.assertEqual(len(api.results), before + 1)
                self.assertNotIn('SECRET', str(failure.exception))

    @staticmethod
    def cli(path, *arguments):
        return subprocess.run([sys.executable, str(Path(collector.__file__)), '--config', str(path), *arguments],
                              capture_output=True, text=True, timeout=10)


if __name__ == '__main__':
    unittest.main()
