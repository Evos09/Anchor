import unittest

from durations import SECONDS


class ExistingTest(unittest.TestCase):
    def test_units(self):
        self.assertEqual(SECONDS['m'], 60)
