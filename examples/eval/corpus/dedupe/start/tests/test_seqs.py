import unittest

from seqs import count


class ExistingTest(unittest.TestCase):
    def test_count(self):
        self.assertEqual(count([1, 2, 1], 1), 2)
