import io
import unittest

from logtool.cli import main

LOG = """2024-03-01T10:00:00Z INFO service started
2024-03-01T11:30:00Z WARN disk at 80 percent
2024-03-01T12:15:00Z ERROR request failed
not a log line
2024-03-01T13:00:00Z DEBUG cache stats
"""


def run(argv, text=LOG):
    out = io.StringIO()
    code = main(argv, stdin=io.StringIO(text), stdout=out)
    return code, out.getvalue().splitlines()


class VisibleTests(unittest.TestCase):
    def test_no_options_keeps_every_entry(self):
        code, lines = run([])
        self.assertEqual(code, 0)
        self.assertEqual(len(lines), 4)

    def test_level_keeps_that_level_and_above(self):
        _, lines = run(["--level", "WARN"])
        self.assertEqual([l.split()[1] for l in lines], ["WARN", "ERROR"])

    def test_level_in_lower_case(self):
        _, lines = run(["--level", "error"])
        self.assertEqual(len(lines), 1)

    def test_since_drops_older_entries(self):
        _, lines = run(["--since", "2024-03-01T11:00:00Z"])
        self.assertEqual(len(lines), 3)


if __name__ == "__main__":
    unittest.main()
