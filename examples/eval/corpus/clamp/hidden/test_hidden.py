import unittest

from numeric import sign, clamp


class HiddenTest(unittest.TestCase):
    def test_all(self):
        self.assertEqual(clamp(5, 0, 10), 5)
        self.assertEqual(clamp(0, 0, 10), 0)
        self.assertEqual(clamp(10, 0, 10), 10)
        self.assertEqual(clamp(-1, 0, 10), 0)
        self.assertEqual(clamp(11, 0, 10), 10)
        self.assertEqual(clamp(2.5, 1, 2), 2)
        self.assertEqual(clamp(5, 3, 3), 3)
        with self.assertRaises(ValueError):
            clamp(1, 5, 0)
        self.assertEqual(sign(-2), -1)
