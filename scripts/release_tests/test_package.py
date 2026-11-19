import hashlib
import pathlib
import sys
import tarfile
import tempfile
import unittest
import zipfile

sys.path.insert(0, str(pathlib.Path(__file__).resolve().parents[1]))
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
        (self.root / 'scripts/requirements.txt').write_text('redis>=5')

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
