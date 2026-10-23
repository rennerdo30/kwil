"""Offline installer integration: fake HTTPS transport, real isolated publication."""
import os
import pathlib
import shutil
import subprocess
import tempfile
import unittest

SCRIPT = pathlib.Path(__file__).with_name('install.sh').resolve()
FAKE_CURL = r'''#!/usr/bin/env python3
import hashlib, json, os, pathlib, sys
args = sys.argv[1:]
dest = pathlib.Path(args[args.index('--output') + 1])
url = args[-1]
payload = b'non-executable installer fixture'
if url.endswith('/latest'):
    dest.write_text(json.dumps(dict(draft=False, prerelease=False, tag_name='v1.2.3')))
elif url.endswith('/SHA256SUMS'):
    digest = hashlib.sha256(payload).hexdigest()
    targets = ['x86_64-unknown-linux-gnu', 'aarch64-unknown-linux-gnu', 'x86_64-apple-darwin', 'aarch64-apple-darwin']
    dest.write_text(''.join(f'{digest}  kwil-{target}\n' for target in targets))
else:
    dest.write_bytes(b'corrupt' if os.environ.get('CORRUPT_FIXTURE') else payload)
'''


@unittest.skipIf(os.name == 'nt', 'POSIX installer runs on hosted Linux/macOS')
class Installer(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix='kwil-install-test-')
        self.root = pathlib.Path(self.temp.name)
        tools = self.root / 'tools'
        tools.mkdir()
        curl = tools / 'curl'
        curl.write_text(FAKE_CURL, encoding='utf-8')
        curl.chmod(0o755)
        self.env = dict(os.environ, HOME=str(self.root / 'home'), PATH=str(tools) + os.pathsep + os.environ['PATH'])

    def tearDown(self):
        self.temp.cleanup()

    def run_installer(self, *args):
        child = subprocess.Popen([shutil.which('sh'), str(SCRIPT), *args], env=self.env,
                                 stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
        print(f'installer fixture PID {child.pid}', flush=True)
        try:
            out, err = child.communicate(timeout=15)
        except subprocess.TimeoutExpired:
            child.kill()
            child.communicate()
            raise
        return child.returncode, out + err

    def test_exact_bytes_and_existing_launcher_preserved(self):
        code, output = self.run_installer()
        self.assertEqual(code, 0, output)
        launcher = self.root / 'home/.local/bin/kwil'
        self.assertTrue(launcher.is_symlink())
        self.assertEqual(launcher.read_bytes(), b'non-executable installer fixture')
        code, output = self.run_installer()
        self.assertNotEqual(code, 0)
        self.assertIn('retained', output)
        self.assertEqual(launcher.read_bytes(), b'non-executable installer fixture')

    def test_corruption_never_publishes(self):
        self.env['CORRUPT_FIXTURE'] = '1'
        code, output = self.run_installer()
        self.assertNotEqual(code, 0)
        self.assertIn('Checksum mismatch', output)
        self.assertFalse((self.root / 'home/.local/bin/kwil').exists())
        self.assertEqual(list((self.root / 'home/.local/share/kwil').iterdir()), [])

    def test_invalid_version_refused_before_install(self):
        code, output = self.run_installer('v1.2.3/../../escape')
        self.assertNotEqual(code, 0)
        self.assertIn('Expected latest', output)
        self.assertFalse((self.root / 'home').exists())


if __name__ == '__main__':
    unittest.main()
