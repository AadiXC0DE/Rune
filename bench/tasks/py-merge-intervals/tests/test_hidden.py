import unittest

from intervals import merge


class HiddenMergeTest(unittest.TestCase):
    def test_single_interval(self):
        self.assertEqual(merge([(4, 9)]), [(4, 9)])

    def test_nested_intervals(self):
        self.assertEqual(merge([(0, 20), (5, 10), (6, 7), (12, 15)]), [(0, 20)])

    def test_negative_bounds(self):
        self.assertEqual(merge([(-5, -1), (-3, 2), (4, 6)]), [(-5, 2), (4, 6)])

    def test_point_intervals(self):
        self.assertEqual(merge([(3, 3), (3, 3), (5, 5)]), [(3, 3), (5, 5)])

    def test_input_with_lists_is_left_alone(self):
        data = [(2, 3), (1, 2)]
        merge(data)
        self.assertEqual(data, [(2, 3), (1, 2)])


if __name__ == "__main__":
    unittest.main()
