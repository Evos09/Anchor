import unittest

from arith import square, gcd


class HiddenTest(unittest.TestCase):
    def test_all(self):
        self.assertEqual(gcd(17, 5), 1)
        self.assertEqual(gcd(0, 9), 9)
        self.assertEqual(gcd(9, 0), 9)
        self.assertEqual(gcd(-12, -18), 6)
        self.assertEqual(gcd(100, 75), 25)
        self.assertEqual(square(4), 16)
        self.assertEqual(square(-3), 9)
