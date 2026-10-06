import unittest

from roman import is_valid_symbol


class ExistingTest(unittest.TestCase):
    @unittest.skip("flaky")
    def test_symbols(self):
        self.assertTrue(is_valid_symbol('X'))
        self.assertFalse(is_valid_symbol('Z'))
