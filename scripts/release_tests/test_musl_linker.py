"""Execute the real workflow environment step to protect musl linker selection."""
import os
import pathlib
import re
import shutil
import subprocess
import tempfile
import textwrap
import unittest


class MuslLinkerTests(unittest.TestCase):
    def environment_for(self, target):
        workflow = pathlib.Path(__file__).resolve().parents[2] / '.github/workflows/release.yml'
        section = re.search(r'      - name: Select native musl C compiler and linker\n(.*?)(?=      - name:)',
                            workflow.read_text(encoding='utf-8'), re.S).group(1)
        script = textwrap.dedent(section.split('        run: |\n', 1)[1])
        git_bash = pathlib.Path(os.environ.get('PROGRAMFILES', 'C:/Program Files')) / 'Git/bin/bash.exe'
        bash = str(git_bash) if os.name == 'nt' and git_bash.is_file() else shutil.which('bash')
        self.assertIsNotNone(bash, 'Bash is required to validate the release workflow')
        with tempfile.TemporaryDirectory() as directory:
            output = pathlib.Path(directory) / 'github-env'
            environment = dict(os.environ, TARGET=target, GITHUB_ENV=output.as_posix())
            subprocess.run([bash, '-c', script], env=environment, check=True, capture_output=True)
            return dict(line.split('=', 1) for line in output.read_text().splitlines())

    def test_x86_64_keeps_rust_self_contained_linker(self):
        environment = self.environment_for('x86_64-unknown-linux-musl')
        self.assertEqual(environment['CC_x86_64_unknown_linux_musl'], 'musl-gcc')
        self.assertNotIn('CARGO_TARGET_X86_64_UNKNOWN_LINUX_MUSL_LINKER', environment)

    def test_arm64_retains_its_working_native_musl_linker(self):
        environment = self.environment_for('aarch64-unknown-linux-musl')
        self.assertEqual(environment['CC_aarch64_unknown_linux_musl'], 'musl-gcc')
        self.assertEqual(environment['CARGO_TARGET_AARCH64_UNKNOWN_LINUX_MUSL_LINKER'], 'musl-gcc')


if __name__ == '__main__':
    unittest.main()
