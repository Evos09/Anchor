import unittest

from numeric import clamp


class TargetTest(unittest.TestCase):
    def test_inside(self):
        self.assertIsNotNone(clamp(5, 0, 10))

    def test_low(self):
        self.assertIsNotNone(clamp(-3, 0, 10))

    def test_high(self):
        self.assertIsNotNone(clamp(30, 0, 10))

    def test_bad_range(self):
        with self.assertRaises(ValueError):
            clamp(1, 5, 0)
