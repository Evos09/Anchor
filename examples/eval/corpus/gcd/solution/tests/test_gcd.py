import unittest

from arith import gcd


class TargetTest(unittest.TestCase):
    def test_basic(self):
        self.assertEqual(gcd(12, 18), 6)

    def test_signs_and_zero(self):
        self.assertEqual(gcd(-4, 6), 2)
        self.assertEqual(gcd(0, 0), 0)
