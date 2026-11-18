import contextlib
import importlib.util
import io
from pathlib import Path
import sys
import tempfile
import unittest
from unittest import mock


spec = importlib.util.spec_from_file_location('retry_build', Path(__file__).with_name('retry_build.py'))
retry_build = importlib.util.module_from_spec(spec)
spec.loader.exec_module(retry_build)

# Verbatim from the Windows x86_64 job of rootshell release run 44172397046.
WINDOWS_READ_FAILURE = (
    r'  D:\a\herdr\herdr\vendor\libghostty-vt\pkg/wuffs\build.zig.zon:14:20: '
    'error: HTTP response read failure: Unexpected'
)
UNIX_CONNECT_FAILURE = (
    '  /home/runner/work/herdr/herdr/vendor/libghostty-vt/build.zig.zon:14:20: '
    'error: unable to connect to server: ConnectionTimedOut'
)
HASH_MISMATCH = (
    '  /home/runner/work/herdr/herdr/vendor/libghostty-vt/pkg/wuffs/build.zig.zon:15:21: '
    'error: hash mismatch: manifest declares N-V-__8AAP5J but the fetched package has N-V-__8AAAAA'
)


class BuildRetryTests(unittest.TestCase):
    def exercise(self, message, failures, expected_exit, expected_attempts):
        with tempfile.TemporaryDirectory() as folder:
            root = Path(folder)
            child = root / 'build.py'
            counter = root / 'attempts'
            child.write_text('''import pathlib, sys
counter = pathlib.Path(sys.argv[1])
attempt = int(counter.read_text()) + 1 if counter.exists() else 1
counter.write_text(str(attempt))
print('build progress', flush=True)
if attempt <= int(sys.argv[2]):
    print(sys.argv[3], file=sys.stderr, flush=True)
    sys.exit(23)
''', encoding='utf-8')
            output = io.StringIO()
            with contextlib.redirect_stdout(output), mock.patch.object(retry_build.time, 'sleep') as sleep:
                code = retry_build.run_build([sys.executable, str(child), str(counter), str(failures), message])
            self.assertEqual(code, expected_exit)
            self.assertEqual(int(counter.read_text()), expected_attempts)
            self.assertEqual(sleep.call_count, expected_attempts - 1)
            self.assertEqual(
                [call.args[0] for call in sleep.call_args_list],
                list(retry_build.DELAYS[:expected_attempts - 1]),
            )
            self.assertIn('build progress', output.getvalue())
            if failures:
                self.assertIn(message, output.getvalue())

    def test_recovers_from_transient_zig_http_read_failure(self):
        self.exercise(WINDOWS_READ_FAILURE, 2, 0, 3)

    def test_recovers_from_zig_connection_failure(self):
        self.exercise(UNIX_CONNECT_FAILURE, 1, 0, 2)

    def test_bounds_persistent_network_failures(self):
        self.exercise(WINDOWS_READ_FAILURE, 99, 23, retry_build.ATTEMPTS)

    def test_does_not_retry_compiler_errors(self):
        self.exercise('error[E0425]: cannot find value', 99, 23, 1)

    def test_does_not_retry_checksum_failures(self):
        self.exercise(HASH_MISMATCH, 99, 23, 1)

    def test_success_runs_once(self):
        self.exercise('', 0, 0, 1)


if __name__ == '__main__':
    unittest.main()
