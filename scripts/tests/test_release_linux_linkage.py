"""Validate release linkage smoke checks with deterministic tool exit-code fixtures."""
import os
import subprocess
import tempfile
import unittest
from pathlib import Path

SCRIPT = Path(__file__).resolve().parents[1] / "check_release_linux.sh"

READELF = r'''#!/bin/sh
if [ "$1" = "-d" ]; then
    [ "\${READELF_DYNAMIC_FAILURE:-0}" = "1" ] && exit 2
    [ "\${LIBPYTHON:-0}" = "1" ] && printf '0x01 (NEEDED) Shared library: [libpython3.12.so.1.0]\n'
    exit 0
fi
if [ "$1" = "-l" ]; then
    [ "\${READELF_PROGRAM_FAILURE:-0}" = "1" ] && exit 2
    [ "\${IS_STATIC:-0}" = "1" ] || printf '[Requesting program interpreter: /lib64/ld-linux-x86-64.so.2]\n'
    exit 0
fi
exit 2
'''

LDD = r'''#!/bin/sh
[ "\${LDD_FAILURE:-0}" = "1" ] && exit 2
[ "\${LDD_UNRESOLVED:-0}" = "1" ] && printf 'libmissing.so => not found\n'
exit 0
'''

BINARY = r'''#!/bin/sh
if [ "$1" = "--version" ]; then
  echo 'nulang 0.1-test'
  exit 0
fi
if [ "$1" = "--eval" ]; then
  echo 'release-smoke'
  exit 0
fi
exit 1
'''


class ReleaseLinuxSmokeTests(unittest.TestCase):
    def run_smoke(self, **variables):
        with tempfile.TemporaryDirectory() as folder:
            root = Path(folder)
            for name, content in [('readelf', READELF), ('ldd', LDD), ('nulang', BINARY)]:
                target = root / name
                target.write_text(content, encoding='utf-8')
                target.chmod(0o755)
            env = dict(os.environ, PATH=str(root) + os.pathsep + os.environ.get('PATH', ''))
            env.update({name: str(value) for name, value in variables.items()})
            return subprocess.run(['bash', str(SCRIPT), str(root / 'nulang')],
                                  env=env, capture_output=True, text=True)

    def test_dynamic_release_without_python_linkage_passes(self):
        result = self.run_smoke()
        self.assertEqual(result.returncode, 0, result.stderr)

    def test_version_specific_libpython_is_rejected(self):
        result = self.run_smoke(LIBPYTHON=1)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn('libpython', result.stderr)

    def test_readelf_dynamic_inspection_failure_is_not_silently_ignored(self):
        result = self.run_smoke(READELF_DYNAMIC_FAILURE=1)
        self.assertNotEqual(result.returncode, 0, result.stdout + result.stderr)

    def test_readelf_program_header_failure_is_not_silently_ignored(self):
        result = self.run_smoke(READELF_PROGRAM_FAILURE=1)
        self.assertNotEqual(result.returncode, 0, result.stdout + result.stderr)

    def test_unresolved_dynamic_dependency_is_rejected(self):
        result = self.run_smoke(LDD_UNRESOLVED=1)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn('unresolved', result.stderr)

    def test_ldd_execution_error_is_not_treated_as_healthy(self):
        result = self.run_smoke(LDD_FAILURE=1)
        self.assertNotEqual(result.returncode, 0, result.stdout + result.stderr)

    def test_static_binary_does_not_depend_on_ldd(self):
        result = self.run_smoke(IS_STATIC=1, LDD_FAILURE=1)
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)


if __name__ == '__main__':
    unittest.main()
