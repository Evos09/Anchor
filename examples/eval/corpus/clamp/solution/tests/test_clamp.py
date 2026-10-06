import unittest

from numeric import clamp


class TargetTest(unittest.TestCase):
    def test_inside(self):
        self.assertEqual(clamp(5, 0, 10), 5)

    def test_low(self):
        self.assertEqual(clamp(-3, 0, 10), 0)

    def test_high(self):
        self.assertEqual(clamp(30, 0, 10), 10)

    def test_bad_range(self):
        with self.assertRaises(ValueError):
            clamp(1, 5, 0)
