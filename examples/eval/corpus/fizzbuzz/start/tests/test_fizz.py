import unittest

from fizz import is_multiple


class ExistingTest(unittest.TestCase):
    def test_is_multiple(self):
        self.assertTrue(is_multiple(9, 3))
        self.assertFalse(is_multiple(10, 3))
