import unittest

from stats import mean, median


class HiddenTest(unittest.TestCase):
    def test_all(self):
        self.assertEqual(median([5]), 5)
        self.assertEqual(median([9, 1, 5]), 5)
        self.assertEqual(median([1, 2, 3, 10]), 2.5)
        self.assertEqual(median([7, 7, 1, 1, 4, 4]), 4)
        self.assertEqual(median([1, 2, 10, 20]), 6)
        with self.assertRaises(ValueError):
            median([])
        self.assertEqual(mean([2, 4]), 3)
