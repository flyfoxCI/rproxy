import pathlib
import unittest

import yaml

ROOT = pathlib.Path(__file__).resolve().parents[2]


class DeploymentTests(unittest.TestCase):
    def test_compose_has_two_services_with_readonly_config_and_durable_state(self):
        compose = yaml.safe_load((ROOT / 'compose.yaml').read_text(encoding='utf-8'))
        self.assertEqual(set(compose['services']), {'rproxy', 'collector'})
        rust = compose['services']['rproxy']
        collector = compose['services']['collector']
        self.assertEqual(rust['build']['target'], 'rproxy')
        self.assertEqual(collector['build']['target'], 'collector')
        for service, path in [(rust, '/etc/rproxy/rproxy.toml'), (collector, '/etc/rproxy/proxy.toml')]:
            mount = next(volume for volume in service['volumes'] if volume['target'] == path)
            self.assertTrue(mount['read_only'])
            self.assertFalse(mount['bind']['create_host_path'])
            self.assertEqual(service['user'], '10001:10001')
        state = next(volume for volume in rust['volumes'] if volume['target'] == '/var/lib/rproxy')
        self.assertEqual(state['type'], 'volume')
        self.assertFalse(any('9100' in str(port) for port in rust['ports']))
        self.assertFalse(collector.get('ports'))
        self.assertTrue(all('127.0.0.1' in port for port in rust['ports']))
        self.assertIn('healthcheck', rust)
        command = rust['command']
        self.assertIn('/var/lib/rproxy/rproxy.sqlite3', command)
        self.assertIn('0.0.0.0:9100', command)
        self.assertIn('0.0.0.0:9099', command)
        self.assertIn('0.0.0.0:9089', command)
        self.assertIn('--admin-bind', rust['healthcheck']['test'])
        self.assertIn('healthcheck', rust['healthcheck']['test'])
        self.assertEqual(collector['depends_on']['rproxy']['condition'], 'service_healthy')
        self.assertIn('RPROXY_ADMIN_TOKEN', collector['environment'])
        self.assertNotIn('RPROXY_PASSWORD', collector['environment'])
        self.assertTrue(collector['env_file'])
        for service in (rust, collector):
            self.assertEqual(service['restart'], 'unless-stopped')
            self.assertTrue(service['read_only'])
            self.assertEqual(service['cap_drop'], ['ALL'])
        self.assertFalse(any(volume['target'].endswith('/certs') for volume in rust['volumes']))

    def test_build_context_is_an_allowlist_without_credentials(self):
        rules = (ROOT / '.dockerignore').read_text(encoding='utf-8').splitlines()
        self.assertIn('**', rules)
        for forbidden in ['!.env', '!rproxy.toml', '!scripts/proxy.toml', '!data/**', '!certs/**']:
            self.assertNotIn(forbidden, rules)
        dockerfile = (ROOT / 'Dockerfile').read_text(encoding='utf-8')
        self.assertNotIn('COPY . ', dockerfile)
        self.assertIn('AS rproxy', dockerfile)
        self.assertIn('AS collector', dockerfile)
        self.assertNotIn('redis', dockerfile.lower())

    def test_https_is_opt_in_with_readonly_certificates(self):
        override = yaml.safe_load((ROOT / 'compose.https.yaml').read_text(encoding='utf-8'))
        service = override['services']['rproxy']
        cert = service['volumes'][0]
        self.assertEqual(cert['target'], '/etc/rproxy/certs')
        self.assertTrue(cert['read_only'])
        self.assertFalse(cert['bind']['create_host_path'])
        self.assertTrue(service['ports'][0].endswith(':9443'))
        self.assertIn('127.0.0.1', service['ports'][0])


if __name__ == '__main__':
    unittest.main()
