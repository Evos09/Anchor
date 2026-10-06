import unittest

from strings import reverse, is_palindrome


class HiddenTest(unittest.TestCase):
    def test_all(self):
        self.assertTrue(is_palindrome(""))
        self.assertTrue(is_palindrome("No 'x' in Nixon"))
        self.assertTrue(is_palindrome("12321"))
        self.assertFalse(is_palindrome("123 21x"))
        self.assertFalse(is_palindrome("ab"))
        self.assertEqual(reverse("ab"), "ba")
