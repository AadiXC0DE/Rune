import unittest

from numerals import from_roman, to_roman


class VisibleTests(unittest.TestCase):
    def test_plain_additive_numbers(self):
        self.assertEqual(to_roman(3), "III")
        self.assertEqual(to_roman(58), "LVIII")
        self.assertEqual(from_roman("VIII"), 8)

    def test_subtractive_forms_to_roman(self):
        self.assertEqual(to_roman(4), "IV")
        self.assertEqual(to_roman(9), "IX")
        self.assertEqual(to_roman(40), "XL")

    def test_subtractive_forms_from_roman(self):
        self.assertEqual(from_roman("IV"), 4)
        self.assertEqual(from_roman("XIV"), 14)


if __name__ == "__main__":
    unittest.main()
