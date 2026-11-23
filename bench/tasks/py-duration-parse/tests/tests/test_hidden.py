import unittest

from durations import format_duration, parse_duration


class HiddenTests(unittest.TestCase):
    def test_zero(self):
        self.assertEqual(parse_duration("0s"), 0)
        self.assertEqual(format_duration(0), "0s")

    def test_mixed_units(self):
        self.assertEqual(parse_duration("1d1s"), 86401)
        self.assertEqual(parse_duration("2d3h4m5s"), 2 * 86400 + 3 * 3600 + 4 * 60 + 5)

    def test_rejects_bad_text(self):
        for bad in ("1x", "h", "-5m", "", "10", "5m!", "m5", "1h 30m", "1.5h"):
            with self.assertRaises(ValueError, msg=bad):
                parse_duration(bad)

    def test_format_values(self):
        self.assertEqual(format_duration(90), "1m30s")
        self.assertEqual(format_duration(86400), "1d")
        self.assertEqual(format_duration(3661), "1h1m1s")
        self.assertEqual(format_duration(90000), "1d1h")

    def test_format_rejects_negative(self):
        with self.assertRaises(ValueError):
            format_duration(-1)

    def test_round_trip(self):
        for n in (0, 1, 59, 60, 61, 3599, 3600, 86399, 86400, 123456, 10**7):
            self.assertEqual(parse_duration(format_duration(n)), n)


if __name__ == "__main__":
    unittest.main()
