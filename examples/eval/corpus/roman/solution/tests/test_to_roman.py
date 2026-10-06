import unittest

from roman import to_roman


class TargetTest(unittest.TestCase):
    def test_small(self):
        self.assertEqual(to_roman(3), "III")
        self.assertEqual(to_roman(4), "IV")
        self.assertEqual(to_roman(9), "IX")

    def test_large(self):
        self.assertEqual(to_roman(1994), "MCMXCIV")

    def test_range(self):
        with self.assertRaises(ValueError):
            to_roman(0)
