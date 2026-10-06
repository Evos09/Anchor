import unittest

from search import is_sorted


class ExistingTest(unittest.TestCase):
    def test_sorted(self):
        self.assertTrue(is_sorted([1, 2, 2, 5]))
        self.assertFalse(is_sorted([3, 1]))
