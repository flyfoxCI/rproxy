import hashlib
import pathlib
import sys
import tarfile
import tempfile
import unittest
from unittest import mock
import zipfile

sys.path.insert(0, str(pathlib.Path(__file__).resolve().parents[1]))
import package_release
from package_release import build_archive, write_checksums


class ReleasePackagingTests(unittest.TestCase):
    def setUp(self):
        self.directory = tempfile.TemporaryDirectory()
        self.root = pathlib.Path(self.directory.name)
        (self.root / 'Cargo.toml').write_text('[package]\nversion="0.2.0"\n')
        (self.root / 'README.md').write_text('usage')
        (self.root / 'docs').mkdir()
        (self.root / 'docs/rproxy-cli.md').write_text('CLI usage')
        (self.root / 'docs/collector-management.md').write_text('Collector worker usage')
        (self.root / '.env').write_text('PRIVATE ENVIRONMENT')
        (self.root / 'data').mkdir()
        (self.root / 'data/state.sqlite3').write_bytes(b'PRIVATE DATABASE')
        (self.root / 'rproxy.example.toml').write_text('example configuration')
        (self.root / 'sources.example.toml').write_text('schema_version=1\nsources=[]')
        (self.root / 'rproxy.toml').write_text('PRIVATE PASSWORD')
        (self.root / 'scripts').mkdir()
        (self.root / 'scripts/crawl.py').write_text('print("maintenance")')
        (self.root / 'scripts/requirements.txt').write_text('requests>=2.32')
        (self.root / 'scripts/proxy.example.toml').write_text('[rproxy]')
        (self.root / 'scripts/providers.env.example').write_text('# supplier environment')
        (self.root / 'scripts/proxy.toml').write_text('PRIVATE SUPPLIER SECRET')

    def tearDown(self):
        self.directory.cleanup()

    def executable(self, target):
        binary = self.root / 'target' / target / 'release' / ('rproxy.exe' if 'windows' in target else 'rproxy')
        binary.parent.mkdir(parents=True)
        binary.write_bytes(b'test executable')
        cli = binary.with_name('rproxy-cli.exe' if 'windows' in target else 'rproxy-cli')
        cli.write_bytes(b'test cli executable')

    def test_windows_zip_contains_only_distributable_files(self):
        target = 'x86_64-pc-windows-msvc'
        self.executable(target)
        archive = build_archive(self.root, target, 'v0.2.0', self.root / 'dist')
        self.assertEqual(archive.name, 'rproxy-v0.2.0-x86_64-pc-windows-msvc.zip')
        with zipfile.ZipFile(archive) as bundle:
            names = bundle.namelist()
            prefix = 'rproxy-v0.2.0-x86_64-pc-windows-msvc/'
            self.assertIn(prefix + 'rproxy.exe', names)
            self.assertIn(prefix + 'rproxy-cli.exe', names)
            self.assertEqual(bundle.read(prefix + 'rproxy-cli.exe'), b'test cli executable')
            self.assertIn(prefix + 'scripts/crawl.py', names)
            self.assertNotIn(prefix + 'rproxy.toml', names)
            self.assertNotIn(prefix + '.env', names)
            self.assertFalse(any('/data/' in name for name in names))
            self.assertIn(prefix + 'docs/rproxy-cli.md', names)
            self.assertIn(prefix + 'docs/collector-management.md', names)
            self.assertIn(prefix + 'sources.example.toml', names)
            self.assertIn(prefix + 'scripts/proxy.example.toml', names)
            self.assertIn(prefix + 'scripts/providers.env.example', names)
            self.assertNotIn(prefix + 'scripts/proxy.toml', names)
            self.assertEqual(bundle.read(prefix + 'rproxy.exe'), b'test executable')

    def test_linux_tar_has_executable_mode_and_matching_checksum(self):
        target = 'x86_64-unknown-linux-musl'
        self.executable(target)
        archive = build_archive(self.root, target, 'v0.2.0', self.root / 'dist')
        with tarfile.open(archive, 'r:gz') as bundle:
            binary = bundle.getmember('rproxy-v0.2.0-x86_64-unknown-linux-musl/rproxy')
            self.assertEqual(binary.mode & 0o777, 0o755)
            cli = bundle.getmember('rproxy-v0.2.0-x86_64-unknown-linux-musl/rproxy-cli')
            self.assertEqual(cli.mode & 0o777, 0o755)
        checksums = write_checksums(self.root / 'dist')
        expected = hashlib.sha256(archive.read_bytes()).hexdigest() + '  ' + archive.name + '\n'
        self.assertEqual(checksums.read_text(), expected)

    def test_linux_and_macos_packages_cover_arm64_and_x86_64(self):
        for target in ['x86_64-unknown-linux-musl', 'aarch64-unknown-linux-musl',
                       'x86_64-apple-darwin', 'aarch64-apple-darwin']:
            with self.subTest(target=target):
                self.executable(target)
                archive = build_archive(self.root, target, 'v0.2.0', self.root / 'dist')
                self.assertEqual(archive.name, f'rproxy-v0.2.0-{target}.tar.gz')
                with tarfile.open(archive, 'r:gz') as bundle:
                    member = bundle.getmember(f'rproxy-v0.2.0-{target}/rproxy')
                    self.assertEqual(member.mode & 0o777, 0o755)
                    self.assertEqual(bundle.extractfile(member).read(), b'test executable')
                    cli = bundle.getmember(f'rproxy-v0.2.0-{target}/rproxy-cli')
                    self.assertEqual(cli.mode & 0o777, 0o755)
                    self.assertEqual(bundle.extractfile(cli).read(), b'test cli executable')

    def test_publication_requires_all_five_platform_archives_without_extras(self):
        targets = ['x86_64-pc-windows-msvc', 'x86_64-unknown-linux-musl',
                   'aarch64-unknown-linux-musl', 'x86_64-apple-darwin', 'aarch64-apple-darwin']
        output = self.root / 'dist'
        for index, target in enumerate(targets):
            self.executable(target)
            build_archive(self.root, target, 'v0.2.0', output)
            if index < 4:
                with self.assertRaises(ValueError):
                    package_release.verify_archives(output, 'v0.2.0')
        names = {path.name for path in package_release.verify_archives(output, 'v0.2.0')}
        self.assertEqual(names, {
            'rproxy-v0.2.0-x86_64-pc-windows-msvc.zip',
            'rproxy-v0.2.0-x86_64-unknown-linux-musl.tar.gz',
            'rproxy-v0.2.0-aarch64-unknown-linux-musl.tar.gz',
            'rproxy-v0.2.0-x86_64-apple-darwin.tar.gz',
            'rproxy-v0.2.0-aarch64-apple-darwin.tar.gz',
        })
        (output / 'rproxy-v0.1.0-x86_64-apple-darwin.tar.gz').write_bytes(b'unexpected old archive')
        with self.assertRaises(ValueError):
            package_release.verify_archives(output, 'v0.2.0')

    def test_publication_gate_inspects_both_binaries_in_all_five_archives(self):
        output = self.root / 'dist'
        for target in sorted(package_release.TARGETS):
            self.executable(target)
            build_archive(self.root, target, 'v0.2.0', output)
        archive = output / 'rproxy-v0.2.0-x86_64-pc-windows-msvc.zip'
        with zipfile.ZipFile(archive) as bundle:
            contents = [(name, bundle.read(name)) for name in bundle.namelist()
                        if not name.endswith('/rproxy-cli.exe')]
        with zipfile.ZipFile(archive, 'w') as bundle:
            for name, content in contents:
                bundle.writestr(name, content)
        with self.assertRaisesRegex(ValueError, 'both binaries'):
            package_release.verify_archives(output, 'v0.2.0')

    def test_main_packaging_checks_native_cli_version_before_archive(self):
        target = 'x86_64-pc-windows-msvc'
        self.executable(target)
        with mock.patch.object(package_release.subprocess, 'run') as run, mock.patch.object(package_release, 'native_management_smoke', create=True) as smoke:
            run.return_value = mock.Mock(returncode=0, stdout='rproxy-cli 0.2.0\n')
            archive = package_release.package_native(self.root, target, 'v0.2.0', self.root / 'dist')
            smoke.assert_called_once_with(self.root / 'target' / target / 'release/rproxy.exe', self.root / 'target' / target / 'release/rproxy-cli.exe')
            self.assertTrue(archive.is_file())
            self.assertEqual(run.call_args.args[0], [str(self.root / 'target' / target / 'release/rproxy-cli.exe'), '--version'])
            self.assertEqual(run.call_args.kwargs['timeout'], 10)
        for version in ['rproxy-cli 0.1.0\n', 'rproxy 0.2.0\n']:
            with mock.patch.object(package_release.subprocess, 'run') as run:
                run.return_value = mock.Mock(returncode=0, stdout=version)
                with self.assertRaises(ValueError):
                    package_release.package_native(self.root, target, 'v0.2.0', self.root / 'bad-dist')
        self.assertFalse((self.root / 'bad-dist').exists())

    def test_native_startup_error_preserves_reason_and_redacts_generated_token(self):
        target = 'x86_64-pc-windows-msvc'
        self.executable(target)
        observed = {}
        process = mock.Mock()
        process.poll.return_value = 31
        process.wait.return_value = 31
        def exited(*args, **kwargs):
            observed['token'] = kwargs['env']['RPROXY_ADMIN_TOKEN']
            kwargs['stderr'].write('Error: database is locked; token=' + observed['token'] + '\n')
            kwargs['stderr'].flush()
            return process
        with mock.patch.object(package_release.subprocess, 'Popen', side_effect=exited):
            with self.assertRaises(ValueError) as failure:
                package_release.native_management_smoke(self.root / 'target' / target / 'release/rproxy.exe', self.root / 'target' / target / 'release/rproxy-cli.exe')
        self.assertIn('database is locked', str(failure.exception))
        self.assertIn('31', str(failure.exception))
        self.assertNotIn(observed['token'], str(failure.exception))

    def test_diagnostic_ref_runs_native_checks_but_never_creates_archive(self):
        target = 'x86_64-pc-windows-msvc'
        self.executable(target)
        with mock.patch.object(package_release.subprocess, 'run', return_value=mock.Mock(returncode=0, stdout='rproxy-cli 0.2.0\n')), mock.patch.object(package_release, 'native_management_smoke') as smoke:
            with self.assertRaisesRegex(ValueError, 'diagnostic-only'):
                package_release.package_native(self.root, target, 'diagnostic/macos-native-startup', self.root / 'dist')
            smoke.assert_called_once()
        self.assertFalse((self.root / 'dist').exists())

    def test_failed_native_management_startup_cannot_produce_archive(self):
        target = 'x86_64-pc-windows-msvc'
        self.executable(target)
        with mock.patch.object(package_release.subprocess, 'run', return_value=mock.Mock(returncode=0, stdout='rproxy-cli 0.2.0\n')), mock.patch.object(package_release, 'native_management_smoke', side_effect=ValueError('native state check failed')):
            with self.assertRaisesRegex(ValueError, 'native state check failed'):
                package_release.package_native(self.root, target, 'v0.2.0', self.root / 'dist')
        self.assertFalse((self.root / 'dist').exists())

    def test_archive_builder_never_executes_fake_binaries(self):
        target = 'x86_64-unknown-linux-musl'
        self.executable(target)
        with mock.patch.object(package_release.subprocess, 'run', side_effect=AssertionError('executed fixture')):
            build_archive(self.root, target, 'v0.2.0', self.root / 'dist')

    def test_missing_cli_cannot_produce_archive(self):
        target = 'x86_64-pc-windows-msvc'
        self.executable(target)
        (self.root / 'target' / target / 'release/rproxy-cli.exe').unlink()
        with self.assertRaises(FileNotFoundError):
            build_archive(self.root, target, 'v0.2.0', self.root / 'dist')
        self.assertFalse((self.root / 'dist').exists())

    def test_mismatched_or_invalid_tag_is_rejected(self):
        for tag in ['v0.3.0', '../v0.2.0', 'v0.2.0;bad']:
            with self.assertRaises(ValueError):
                build_archive(self.root, 'x86_64-unknown-linux-musl', tag, self.root / 'dist')
        self.assertFalse((self.root / 'dist').exists())

    def test_missing_binary_cannot_produce_a_release(self):
        with self.assertRaises(FileNotFoundError):
            build_archive(self.root, 'x86_64-unknown-linux-musl', 'v0.2.0', self.root / 'dist')
        with self.assertRaises(ValueError):
            write_checksums(self.root)


if __name__ == '__main__':
    unittest.main()
