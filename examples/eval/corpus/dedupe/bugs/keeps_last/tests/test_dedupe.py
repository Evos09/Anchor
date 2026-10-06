import unittest

from seqs import dedupe


class TargetTest(unittest.TestCase):
    def test_order(self):
        self.assertEqual(len(dedupe([3, 1, 3, 2, 1])), 3)

    def test_empty(self):
        self.assertEqual(dedupe([]), [])
