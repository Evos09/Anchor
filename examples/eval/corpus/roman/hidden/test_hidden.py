import unittest

from roman import is_valid_symbol, to_roman


class HiddenTest(unittest.TestCase):
    def test_all(self):
        cases = {1: "I", 4: "IV", 14: "XIV", 40: "XL", 90: "XC", 400: "CD",
                 944: "CMXLIV", 1994: "MCMXCIV", 3999: "MMMCMXCIX", 2024: "MMXXIV"}
        for n, want in cases.items():
            self.assertEqual(to_roman(n), want, n)
        for bad in (0, 4000, -1):
            with self.assertRaises(ValueError):
                to_roman(bad)
