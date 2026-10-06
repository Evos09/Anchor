import unittest

from search import is_sorted, binary_search


class HiddenTest(unittest.TestCase):
    def test_all(self):
        items = list(range(0, 100, 3))
        for i, v in enumerate(items):
            self.assertEqual(binary_search(items, v), i)
        for v in (-1, 1, 100, 98):
            self.assertEqual(binary_search(items, v), -1)
        self.assertEqual(binary_search([5], 5), 0)
        long = list(range(200))
        self.assertEqual(binary_search(long, 199), 199)
        self.assertEqual(binary_search(long, 77), 77)
        self.assertFalse(is_sorted([3, 1]))
