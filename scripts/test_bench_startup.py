import contextlib
import errno
import importlib.util
import io
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path
from unittest import mock

SPEC = importlib.util.spec_from_file_location(
    "bench_startup", Path(__file__).with_name("bench-startup.py"))
bench = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(bench)

QUIT_TIMEOUT = "quit timed out after the first frame"


class LiveChild:
    """Child that keeps running while wait() always times out."""

    pid = 4242
    returncode = None

    def __init__(self):
        self.waits = []

    def poll(self):
        return None

    def wait(self, timeout=None):
        self.waits.append(timeout)
        if timeout is not None:
            raise subprocess.TimeoutExpired("aoe", timeout)
        return 0


class QuitTimeoutTest(unittest.TestCase):
    def test_wait_timeout_after_eio_is_a_failed_run(self):
        child = LiveChild()
        reads = [bench.FRAME_END]

        def read(fd, size):
            if reads:
                return reads.pop()
            raise OSError(errno.EIO, "slave closed")

        with mock.patch.object(bench.subprocess, "Popen", return_value=child), \
                mock.patch.object(bench.select, "select", return_value=([0], [], [])), \
                mock.patch.object(bench.os, "read", side_effect=read), \
                mock.patch.object(bench.os, "write"), \
                mock.patch.object(bench.os, "killpg") as killpg:
            with self.assertRaisesRegex(RuntimeError, QUIT_TIMEOUT):
                bench.run("aoe", {}, lambda: None, 10)
        # terminate() still ran: SIGTERM, then SIGKILL after its wait timed out.
        self.assertEqual([call.args[1] for call in killpg.call_args_list],
                         [bench.signal.SIGTERM, bench.signal.SIGKILL])

    def test_series_records_fail_continues_and_exits_nonzero(self):
        with tempfile.NamedTemporaryFile() as binary, \
                mock.patch.object(sys, "argv", ["bench", "--binary", binary.name, "--runs", "2"]), \
                mock.patch.object(bench, "fixture"), \
                mock.patch.object(bench, "run",
                                  side_effect=[RuntimeError(QUIT_TIMEOUT), (1.0, 1.0)]) as run, \
                contextlib.redirect_stdout(io.StringIO()) as out:
            status = bench.main()
        self.assertEqual(run.call_count, 2)
        self.assertIn(f"1: FAIL: {QUIT_TIMEOUT}", out.getvalue())
        self.assertIn("2: first_frame=", out.getvalue())
        self.assertEqual(status, 1)


if __name__ == "__main__":
    unittest.main()
