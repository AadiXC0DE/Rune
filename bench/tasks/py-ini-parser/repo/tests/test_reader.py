import unittest

from config import parse


class ReaderTest(unittest.TestCase):
    def test_sections_and_pairs(self):
        self.assertEqual(
            parse("[db]\nhost = localhost\nport=5432\n"),
            {"db": {"host": "localhost", "port": "5432"}},
        )

    def test_value_containing_equals(self):
        self.assertEqual(parse("[a]\nurl = x=1&y=2\n"), {"a": {"url": "x=1&y=2"}})

    def test_comments_full_line_and_inline(self):
        text = "# top\n[a]\n; note\nkey = value  # trailing\nother = v ; also\n"
        self.assertEqual(parse(text), {"a": {"key": "value", "other": "v"}})

    def test_hash_inside_a_quoted_value_is_kept(self):
        self.assertEqual(parse('[a]\ncolor = "#ff0000"\n'), {"a": {"color": "#ff0000"}})

    def test_keys_before_any_section(self):
        self.assertEqual(parse("name = top\n[s]\nk = v\n"), {"": {"name": "top"}, "s": {"k": "v"}})

    def test_later_duplicate_wins(self):
        self.assertEqual(parse("[a]\nk = 1\nk = 2\n"), {"a": {"k": "2"}})

    def test_line_without_equals_is_an_error(self):
        with self.assertRaises(ValueError):
            parse("[a]\njust words\n")


if __name__ == "__main__":
    unittest.main()
