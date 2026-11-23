import unittest

from numerals import from_roman, to_roman


class HiddenTests(unittest.TestCase):
    def test_known_values(self):
        self.assertEqual(to_roman(1994), "MCMXCIV")
        self.assertEqual(from_roman("MCMXCIV"), 1994)
        self.assertEqual(to_roman(3999), "MMMCMXCIX")
        self.assertEqual(from_roman("MMMCMXCIX"), 3999)
        self.assertEqual(to_roman(444), "CDXLIV")
        self.assertEqual(to_roman(900), "CM")

    def test_round_trip_for_every_value(self):
        for n in range(1, 4000):
            self.assertEqual(from_roman(to_roman(n)), n)

    def test_to_roman_rejects_out_of_range(self):
        for bad in (0, -1, 4000, 10000):
            with self.assertRaises(ValueError):
                to_roman(bad)

    def test_from_roman_rejects_invalid_forms(self):
        for bad in ("", "IC", "VV", "IIII", "IXI", "XM", "VX", "MMMM", "IIV", "ABC", "iv"):
            with self.assertRaises(ValueError, msg=bad):
                from_roman(bad)


if __name__ == "__main__":
    unittest.main()
