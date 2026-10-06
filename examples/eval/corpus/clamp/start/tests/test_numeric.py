import unittest

from numeric import sign


class ExistingTest(unittest.TestCase):
    def test_sign(self):
        self.assertEqual(sign(-5), -1)
        self.assertEqual(sign(0), 0)
        self.assertEqual(sign(7), 1)
