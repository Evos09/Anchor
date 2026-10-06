import unittest

from lists import first


class ExistingTest(unittest.TestCase):
    def test_first(self):
        self.assertEqual(first([4, 5]), 4)
        self.assertIsNone(first([]))
