import unittest

from strings import is_palindrome


class TargetTest(unittest.TestCase):
    def test_simple(self):
        self.assertTrue(is_palindrome("level"))
        self.assertFalse(is_palindrome("levels"))

    def test_ignores_case_and_punctuation(self):
        self.assertTrue(is_palindrome("A man, a plan, a canal: Panama"))
