import unittest

from intervals import merge


class MergeTest(unittest.TestCase):
    def test_overlapping(self):
        self.assertEqual(merge([(1, 4), (2, 6)]), [(1, 6)])

    def test_touching_intervals_merge(self):
        self.assertEqual(merge([(1, 3), (3, 5)]), [(1, 5)])

    def test_unsorted_input(self):
        self.assertEqual(merge([(8, 10), (1, 3), (2, 4)]), [(1, 4), (8, 10)])

    def test_contained_interval(self):
        self.assertEqual(merge([(1, 10), (2, 3), (4, 5)]), [(1, 10)])

    def test_empty(self):
        self.assertEqual(merge([]), [])

    def test_input_is_not_modified(self):
        data = [(5, 6), (1, 2)]
        merge(data)
        self.assertEqual(data, [(5, 6), (1, 2)])

    def test_reversed_interval_is_rejected(self):
        with self.assertRaises(ValueError):
            merge([(5, 1)])


if __name__ == "__main__":
    unittest.main()
