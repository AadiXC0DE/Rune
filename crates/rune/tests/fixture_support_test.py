"""Exercise the PTY driver's platform boundaries without launching Rune."""

import errno
import fcntl
import hashlib
import os
import pathlib
import pty
import select
import subprocess
import sys
import termios
import tempfile
import threading
import time
import unittest
from unittest.mock import patch


class TerminalDrainTests(unittest.TestCase):
    def test_provider_condition_drains_output_before_the_signal(self):
        # Rune can repaint more than a queue-full BEFORE the provider sees
        # the submitted line. Reproduce that dependency even on Linux's larger
        # queue; a plain Event.wait/file sleep here cannot let the child finish.
        with tempfile.TemporaryDirectory(prefix="rune-pty-condition-") as directory:
            signal = pathlib.Path(directory) / "request-received"
            master, slave = open_terminal()
            child = subprocess.Popen(
                [sys.executable, "-c",
                 "import pathlib, sys; "
                 "sys.stdout.buffer.write(b'x' * (4 * 1024 * 1024)); "
                 "sys.stdout.buffer.flush(); "
                 "pathlib.Path(sys.argv[1]).touch(); sys.stdin.buffer.read(1)",
                 str(signal)],
                stdin=slave, stdout=slave, stderr=slave,
            )
            os.close(slave)
            transcript = bytearray()
            try:
                wait_for_condition(child, master, transcript, signal.exists,
                                   "provider signal", timeout=10)
                write_terminal(master, b"\n", transcript)
                self.assertEqual(wait_for_exit(child, master, transcript, 10), 0)
                self.assertEqual(transcript, b'x' * (4 * 1024 * 1024) + b'\r\n')
            finally:
                if child.poll() is None:
                    child.kill()
                wait_for_exit(child, master, transcript, 10)
                close_terminal(master)

    def test_condition_deadline_reports_what_was_drained(self):
        master, slave = open_terminal()
        try:
            os.write(slave, b"blocked before request")
            transcript = bytearray()
            child = unittest.mock.Mock()
            child.poll.return_value = None
            with self.assertRaisesRegex(AssertionError, "provider signal") as raised:
                wait_for_condition(child, master, transcript, lambda: False,
                                   "provider signal", timeout=0.05)
            self.assertIn("blocked before request", str(raised.exception))
        finally:
            os.close(slave)
            close_terminal(master)

    def test_key_delivery_pumps_output_and_preserves_every_byte(self):
        master, slave = open_terminal()
        # The child reads a small input chunk, writes a much larger repaint,
        # then reads again. A blocking write-all without reads deadlocks once
        # both queues fill. Raw mode is established before the driver sends.
        import tty
        tty.setraw(slave)
        payload = bytes(range(256)) * 512
        child = subprocess.Popen(
            [sys.executable, "-c",
             "import hashlib, os, sys\n"
             "digest = hashlib.sha256()\n"
             "remaining = int(sys.argv[1])\n"
             "while remaining:\n"
             "    chunk = os.read(0, min(128, remaining))\n"
             "    if not chunk: sys.exit(2)\n"
             "    remaining -= len(chunk)\n"
             "    digest.update(chunk)\n"
             "    sys.stdout.buffer.write(chunk * 32)\n"
             "    sys.stdout.buffer.flush()\n"
             "sys.exit(0 if digest.hexdigest() == sys.argv[2] else 3)\n",
             str(len(payload)), hashlib.sha256(payload).hexdigest()],
            stdin=slave, stdout=slave, stderr=slave,
        )
        os.close(slave)
        transcript = bytearray()
        original_write = os.write
        try:
            # Force short writes as well as real kernel backpressure. Repeating
            # an unsent suffix is byte delivery, never a retry of a test action.
            with patch.object(os, "write", lambda fd, data: original_write(fd, data[:256])):
                write_terminal(master, payload, transcript, timeout=10)
            self.assertTrue(os.get_blocking(master))
            self.assertEqual(wait_for_exit(child, master, transcript, 10), 0)
            # Each read may be shorter than 128 bytes, so compare counts for
            # every byte; each input byte must be returned exactly 32 times.
            self.assertEqual(len(transcript), len(payload) * 32)
            self.assertEqual([transcript.count(value) for value in range(256)], [512 * 32] * 256)
        finally:
            if child.poll() is None:
                child.kill()
            wait_for_exit(child, master, transcript, 10)
            close_terminal(master)

    def test_fixture_environment_ignores_host_scenario_knobs(self):
        with patch.dict(os.environ, {
            "RUNE_ACCESSIBLE": "true", "HTTPS_PROXY": "http://host.invalid",
            "NO_COLOR": "", "COLORTERM": "truecolor", "VISUAL": "host-editor",
            "EDITOR": "host-editor", "COLUMNS": "12", "LINES": "4",
            "PATH": "/fixture/bin",
        }, clear=True):
            self.assertEqual(fixture_environment(), {"PATH": "/fixture/bin"})

    def test_empty_read_eof_preserves_the_final_chunk(self):
        # A closed pipe gives Linux the same readable EOF as a macOS PTY.
        master, writer = os.pipe()
        try:
            os.write(writer, b"last frame")
        finally:
            os.close(writer)
        try:
            transcript = bytearray()
            drain_terminal(master, transcript, timeout=0.2)
            self.assertEqual(transcript, b"last frame")
            # EOF stays readable, including on subsequent drains.
            drain_terminal(master, transcript, timeout=0.2)
            self.assertEqual(transcript, b"last frame")
        finally:
            os.close(master)

    def test_closed_pty_with_both_eof_conventions(self):
        original_read = os.read
        original_select = select.select

        def empty_read_at_eof(fd, size):
            try:
                return original_read(fd, size)
            except OSError as error:
                if error.errno != errno.EIO:
                    raise
                return b""

        for read in (original_read, empty_read_at_eof):
            with self.subTest(read=read.__name__):
                master, slave = pty.openpty()
                try:
                    os.write(slave, b"final frame")
                finally:
                    os.close(slave)
                try:
                    transcript = bytearray()
                    trace = []

                    def trace_select(*args):
                        ready = original_select(*args)
                        trace.append(("ready", bool(ready[0])))
                        return ready

                    def trace_read(fd, size):
                        try:
                            chunk = read(fd, size)
                        except OSError as error:
                            trace.append(("error", error.errno))
                            raise
                        trace.append(("read", chunk))
                        return chunk

                    with patch.object(os, "read", trace_read), patch.object(select, "select", trace_select):
                        drain_terminal(master, transcript, timeout=0.2)
                    print(f"[fixture] closed PTY ({sys.platform}, {read.__name__}): {trace!r}",
                          file=sys.stderr)
                    # This write and last-slave close both happen BEFORE drain.
                    # Darwin ttyclose flushes the unread queue: select reports
                    # disconnection and the FIRST read is empty, permanently.
                    # Linux retains the bytes, then reports EIO (or mapped EOF).
                    # The retained-slave tests below require every byte on both.
                    expected = b"" if sys.platform == "darwin" else b"final frame"
                    self.assertEqual(transcript, expected, trace)
                finally:
                    os.close(master)

    def test_retained_slave_preserves_final_chunk_before_eof(self):
        master, slave = open_terminal()
        try:
            self.assertFalse(os.get_inheritable(_terminal_slaves[master]))
            os.write(slave, b"final frame")
        finally:
            os.close(slave)
        try:
            transcript = bytearray()
            drain_terminal(master, transcript, quiet=0.01, timeout=0.2)
            self.assertEqual(transcript, b"final frame")
            # Only allow the LAST slave close after the output was consumed.
            os.close(_terminal_slaves.pop(master))
            self.assertEqual(_pump(master, transcript, 0.2), b"")
            drain_terminal(master, transcript, timeout=0.2)
            self.assertEqual(transcript, b"final frame")
        finally:
            if master in _terminal_slaves:
                close_terminal(master)
            else:
                os.close(master)

    def test_silent_open_slave_is_quiet_without_eof(self):
        master, slave = pty.openpty()
        try:
            os.write(slave, b"frame")
            transcript = bytearray()
            drain_terminal(master, transcript, quiet=0.01, timeout=0.2)
            self.assertEqual(transcript, b"frame")
            # The descriptor is still open and can produce another frame.
            os.write(slave, b"next")
            drain_terminal(master, transcript, quiet=0.01, timeout=0.2)
            self.assertEqual(transcript, b"framenext")
        finally:
            os.close(slave)
            os.close(master)

    def test_late_final_chunk_is_collected(self):
        master, slave = open_terminal()
        first_read = threading.Event()
        original_read = os.read

        def observe_read(fd, size):
            chunk = original_read(fd, size)
            first_read.set()
            return chunk

        def write_late():
            if first_read.wait(1):
                time.sleep(0.02)
                os.write(slave, b"late frame")
            os.close(slave)

        writer = threading.Thread(target=write_late)
        os.write(slave, b"first frame")
        writer.start()
        try:
            transcript = bytearray()
            with patch.object(os, "read", observe_read):
                drain_terminal(master, transcript, quiet=0.15, timeout=1)
            self.assertEqual(transcript, b"first framelate frame")
        finally:
            writer.join()
            close_terminal(master)

    def test_unrelated_read_errors_are_not_eof(self):
        master, slave = pty.openpty()
        try:
            os.write(slave, b"ready")
            with patch.object(os, "read", side_effect=OSError(errno.EBADF, "bad fd")):
                with self.assertRaises(OSError) as raised:
                    drain_terminal(master, bytearray(), timeout=0.2)
            self.assertEqual(raised.exception.errno, errno.EBADF)
        finally:
            os.close(slave)
            os.close(master)

    def test_shutdown_drains_more_than_a_pty_buffer(self):
        master, slave = open_terminal()

        def controlling_terminal():
            os.setsid()
            fcntl.ioctl(0, termios.TIOCSCTTY, 0)

        child = subprocess.Popen(
            [sys.executable, "-c",
             "import sys; sys.stdout.buffer.write(b'x' * (4 * 1024 * 1024))"],
            stdin=slave, stdout=slave, stderr=slave,
            preexec_fn=controlling_terminal,
        )
        os.close(slave)
        transcript = bytearray()
        try:
            self.assertEqual(wait_for_exit(child, master, transcript, 10), 0)
            # wait_for_exit itself must preserve all bytes, including the tail
            # queued when poll first observes exit; no extra drain repairs it.
            self.assertEqual(len(transcript), 4 * 1024 * 1024)
        finally:
            if child.poll() is None:
                child.kill()
            wait_for_exit(child, master, transcript, 10)
            close_terminal(master)

    def test_shutdown_collects_multiple_chunks_already_queued_at_exit(self):
        master, slave = open_terminal()
        child = subprocess.Popen(
            [sys.executable, "-c", "import os; os.write(1, b'final frame' * 100)"],
            stdin=slave, stdout=slave, stderr=slave,
        )
        os.close(slave)
        original_read = os.read
        original_pump = _pump
        transcript = bytearray()
        queued_chunks = []

        def keep_tail_queued(master, transcript, timeout):
            # Drain the first 1050 bytes while waiting, letting the writer
            # finish even on Darwin's 1024-byte queue. The last 50 bytes fit.
            # Leave that tail queued until poll observes exit, making the
            # post-exit case deterministic even if the reader wins every race.
            if timeout > 0 and len(transcript) >= 1050:
                time.sleep(min(timeout, 0.001))
                return None
            chunk = original_pump(master, transcript, timeout)
            if timeout == 0 and chunk:
                queued_chunks.append(chunk)
            return chunk

        try:
            with patch.object(os, "read", lambda fd, size: original_read(fd, min(size, 7))), \
                    patch.dict(wait_for_exit.__globals__, _pump=keep_tail_queued):
                self.assertEqual(wait_for_exit(child, master, transcript, 1), 0)
            self.assertGreater(len(queued_chunks), 1)
            self.assertGreaterEqual(sum(map(len, queued_chunks)), 50)
            # Exact equality requires every byte, including all queued chunks;
            # no extra drain after wait_for_exit can repair a missing tail.
            self.assertEqual(transcript, b"final frame" * 100)
        finally:
            if child.poll() is None:
                child.kill()
            wait_for_exit(child, master, timeout=1)
            close_terminal(master)


unittest.main(argv=[sys.argv[0]])
