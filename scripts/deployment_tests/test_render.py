"""Check the effective Compose model with the installed Docker/Podman provider."""
import os
from pathlib import Path
import shutil
import subprocess
import unittest

import yaml

ROOT = Path(__file__).resolve().parents[2]
ENGINE = shutil.which('docker') or shutil.which('podman')


@unittest.skipUnless(ENGINE, 'Docker/Podman Compose is not installed')
class ComposeRenderTests(unittest.TestCase):
    def render(self, overrides=None, https=False):
        environment = {key: value for key, value in os.environ.items() if not key.startswith(('RPROXY_', 'COMPOSE_'))}
        environment.update({
            'RPROXY_PASSWORD': 'render-test-password',
            'RPROXY_ADMIN_TOKEN': 'render-test-admin-token',
            'RPROXY_WORKER_TOKEN': 'render-test-worker-token',
            'RPROXY_CONFIG': './rproxy.example.toml',
            'RPROXY_COLLECTOR_CONFIG': './scripts/proxy.example.toml',
            'RPROXY_PROVIDERS_ENV': './scripts/providers.env.example',
            **(overrides or {}),
        })
        command = [ENGINE, 'compose', '--env-file', str(ROOT / '.env.example'), '-f', str(ROOT / 'compose.yaml')]
        if https:
            command += ['-f', str(ROOT / 'compose.https.yaml')]
        return subprocess.run(command + ['config'], cwd=ROOT, env=environment,
                              capture_output=True, text=True, encoding='utf-8', errors='replace', timeout=30)

    def test_effective_defaults_keep_management_private_and_config_readonly(self):
        result = self.render()
        self.assertEqual(result.returncode, 0, result.stderr)
        model = yaml.safe_load(result.stdout)
        service = model['services']['rproxy']
        self.assertEqual({port['target'] for port in service['ports']}, {9089, 9099})
        self.assertTrue(all(port['host_ip'] == '127.0.0.1' for port in service['ports']))
        config = next(mount for mount in service['volumes'] if mount['target'] == '/etc/rproxy/rproxy.toml')
        self.assertTrue(config['read_only'])
        self.assertFalse(config['bind']['create_host_path'])
        self.assertNotIn('RPROXY_PASSWORD', model['services']['collector']['environment'])
        self.assertNotIn('RPROXY_ADMIN_TOKEN', model['services']['collector']['environment'])
        self.assertIn('RPROXY_WORKER_TOKEN', model['services']['collector']['environment'])

    def test_host_ports_and_https_override_are_applied_without_changing_internal_ports(self):
        result = self.render({'RPROXY_HTTP_PORT': '19099', 'RPROXY_HTTPS_PORT': '19443'}, https=True)
        self.assertEqual(result.returncode, 0, result.stderr)
        service = yaml.safe_load(result.stdout)['services']['rproxy']
        ports = {port['target']: str(port['published']) for port in service['ports']}
        self.assertEqual(ports, {9099: '19099', 9089: '9089', 9443: '19443'})
        certs = next(mount for mount in service['volumes'] if mount['target'] == '/etc/rproxy/certs')
        self.assertTrue(certs['read_only'])
        self.assertFalse(certs['bind']['create_host_path'])

    def test_missing_secrets_fail_before_startup(self):
        for name in ('RPROXY_ADMIN_TOKEN', 'RPROXY_WORKER_TOKEN'):
            with self.subTest(name=name):
                result = self.render({name: ''})
                self.assertNotEqual(result.returncode, 0)
                self.assertIn(name, result.stderr)


if __name__ == '__main__':
    unittest.main()
