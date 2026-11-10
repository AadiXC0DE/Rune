import unittest

from config import parse


class HiddenReaderTest(unittest.TestCase):
    def test_equals_and_semicolon_inside_quotes(self):
        self.assertEqual(
            parse('[a]\nq = "x=1; y=2"  ; real comment\n'),
            {"a": {"q": "x=1; y=2"}},
        )

    def test_empty_value(self):
        self.assertEqual(parse("[a]\nk =\nj = \n"), {"a": {"k": "", "j": ""}})

    def test_duplicate_key_keeps_the_last(self):
        self.assertEqual(parse("[a]\nk = 1\nk = 2\nk = 3\n"), {"a": {"k": "3"}})

    def test_equals_in_unquoted_value_with_comment(self):
        self.assertEqual(parse("[a]\nu = a=b=c # note\n"), {"a": {"u": "a=b=c"}})

    def test_global_key_and_section_with_no_keys(self):
        self.assertEqual(parse("g = 1\n[empty]\n"), {"": {"g": "1"}, "empty": {}})


if __name__ == "__main__":
    unittest.main()
