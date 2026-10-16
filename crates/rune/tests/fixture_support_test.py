"""Exercise the PTY driver's platform boundaries without launching Rune."""

import errno
import fcntl
import os
import pty
import select
import subprocess
import sys
import termios
import threading
import time
import unittest
from unittest.mock import patch


class TerminalDrainTests(unittest.TestCase):
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
        try:
            # A small write fits the PTY. Deliberately reap before draining:
            # the retained slave must protect even a child that exits this fast.
            self.assertEqual(child.wait(timeout=1), 0)
            transcript = bytearray()
            with patch.object(os, "read", lambda fd, size: original_read(fd, min(size, 7))):
                self.assertEqual(wait_for_exit(child, master, transcript, 1), 0)
            self.assertEqual(transcript, b"final frame" * 100)
        finally:
            if child.poll() is None:
                child.kill()
            wait_for_exit(child, master, timeout=1)
            close_terminal(master)


unittest.main(argv=[sys.argv[0]])
