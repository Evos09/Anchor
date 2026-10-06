import unittest

from seqs import count, dedupe


class HiddenTest(unittest.TestCase):
    def test_all(self):
        self.assertEqual(dedupe(["b", "a", "b"]), ["b", "a"])
        self.assertEqual(dedupe([1, 1, 1]), [1])
        self.assertEqual(dedupe([2, 1, 2, 1, 3]), [2, 1, 3])
        self.assertEqual(dedupe([0, False, 1]), [0, 1])
        self.assertEqual(dedupe([1, "1"]), [1, "1"])
        desc = list(range(150, 0, -1))
        self.assertEqual(dedupe(desc + desc), desc)
        self.assertEqual(count([1, 1], 1), 2)
