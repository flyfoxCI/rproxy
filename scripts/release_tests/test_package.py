import hashlib
import pathlib
import sys
import tarfile
import tempfile
import unittest
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
        (self.root / 'rproxy.example.toml').write_text('example configuration')
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

    def test_windows_zip_contains_only_distributable_files(self):
        target = 'x86_64-pc-windows-msvc'
        self.executable(target)
        archive = build_archive(self.root, target, 'v0.2.0', self.root / 'dist')
        self.assertEqual(archive.name, 'rproxy-v0.2.0-x86_64-pc-windows-msvc.zip')
        with zipfile.ZipFile(archive) as bundle:
            names = bundle.namelist()
            prefix = 'rproxy-v0.2.0-x86_64-pc-windows-msvc/'
            self.assertIn(prefix + 'rproxy.exe', names)
            self.assertIn(prefix + 'scripts/crawl.py', names)
            self.assertNotIn(prefix + 'rproxy.toml', names)
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
