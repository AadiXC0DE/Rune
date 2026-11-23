import unittest

from durations import format_duration, parse_duration


class VisibleTests(unittest.TestCase):
    def test_parse_plain_units(self):
        self.assertEqual(parse_duration("90s"), 90)
        self.assertEqual(parse_duration("5m"), 300)
        self.assertEqual(parse_duration("1h30m"), 5400)

    def test_parse_days(self):
        self.assertEqual(parse_duration("2d"), 172800)

    def test_garbage_is_refused(self):
        with self.assertRaises(ValueError):
            parse_duration("soon")

    def test_format_round_trip(self):
        self.assertEqual(format_duration(45), "45s")
        self.assertEqual(format_duration(5400), "1h30m")
        self.assertEqual(parse_duration(format_duration(5400)), 5400)


if __name__ == "__main__":
    unittest.main()
