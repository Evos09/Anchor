import unittest

from search import binary_search


class TargetTest(unittest.TestCase):
    def test_found(self):
        self.assertEqual(binary_search([1, 3, 5, 7, 9], 9), 4)
        self.assertEqual(binary_search([1, 3, 5, 7, 9], 1), 0)

    def test_missing(self):
        self.assertEqual(binary_search([1, 3, 5], 4), -1)
        self.assertEqual(binary_search([], 4), -1)
