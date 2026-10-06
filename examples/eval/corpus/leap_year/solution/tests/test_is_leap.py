import unittest

from calendar_util import is_leap


class TargetTest(unittest.TestCase):
    def test_common(self):
        self.assertFalse(is_leap(2023))

    def test_leap(self):
        self.assertTrue(is_leap(2024))

    def test_centuries(self):
        self.assertFalse(is_leap(1900))
        self.assertTrue(is_leap(2000))
