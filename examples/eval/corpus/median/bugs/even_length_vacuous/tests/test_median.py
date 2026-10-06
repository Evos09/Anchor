import unittest

from stats import median


class TargetTest(unittest.TestCase):
    def test_odd(self):
        self.assertEqual(median([3, 1, 2]), 2)

    def test_even(self):
        self.assertTrue(median([4, 1, 3, 2]))

    def test_empty(self):
        with self.assertRaises(ValueError):
            median([])
