import contextlib
import io
import unittest

from logtool.cli import main

LOG = """2024-03-01T10:00:00Z INFO a
2024-03-01T12:00:00Z INFO b
2024-03-01T12:00:01+00:00 WARN c
2024-03-01T14:30:00+02:00 ERROR d
2024-03-01T12:45:00 INFO e
2024-03-01T07:00:00-05:00 DEBUG f
"""


def run(argv, text=LOG):
    out, err = io.StringIO(), io.StringIO()
    try:
        with contextlib.redirect_stderr(err):
            code = main(argv, stdin=io.StringIO(text), stdout=out)
    except SystemExit as exc:
        code = exc.code
    return code, [l.split()[2] for l in out.getvalue().splitlines()], err.getvalue()


class HiddenTests(unittest.TestCase):
    def test_since_with_an_offset(self):
        code, names, _ = run(["--since", "2024-03-01T14:00:00+02:00"])
        self.assertEqual(code, 0)
        self.assertEqual(names, ["b", "c", "d", "e", "f"])

    def test_boundary_entry_is_kept(self):
        _, names, _ = run(["--since", "2024-03-01T12:00:00Z"])
        self.assertEqual(names, ["b", "c", "d", "e", "f"])

    def test_one_second_after_boundary(self):
        _, names, _ = run(["--since", "2024-03-01T12:00:01Z"])
        self.assertEqual(names, ["c", "d", "e"])

    def test_since_without_zone_is_utc(self):
        _, names, _ = run(["--since", "2024-03-01T12:30:00"])
        self.assertEqual(names, ["d", "e"])

    def test_entry_offsets_are_converted(self):
        # f is 12:00 UTC, d is 12:30 UTC
        _, names, _ = run(["--since", "2024-03-01T12:20:00Z"])
        self.assertEqual(names, ["d", "e"])

    def test_since_and_level_together(self):
        _, names, _ = run(["--since", "2024-03-01T12:00:00Z", "--level", "Warn"])
        self.assertEqual(names, ["c", "d"])

    def test_invalid_timestamp_exits_2_with_message(self):
        code, names, err = run(["--since", "yesterday"])
        self.assertEqual(code, 2)
        self.assertEqual(names, [])
        self.assertIn("yesterday", err)

    def test_invalid_level_exits_2(self):
        code, _, err = run(["--level", "loud"])
        self.assertEqual(code, 2)
        self.assertIn("loud", err)


if __name__ == "__main__":
    unittest.main()
