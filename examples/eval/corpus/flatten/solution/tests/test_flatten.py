import unittest

from lists import flatten


class TargetTest(unittest.TestCase):
    def test_flat(self):
        self.assertEqual(flatten([1, 2]), [1, 2])

    def test_nested(self):
        self.assertEqual(flatten([1, [2, [3, [4]]], 5]), [1, 2, 3, 4, 5])
