import unittest

from calendar_util import days_in_week, is_leap


class HiddenTest(unittest.TestCase):
    def test_all(self):
        for y, want in [(2023, False), (2024, True), (1900, False), (2000, True),
                        (2100, False), (2400, True), (1996, True), (1, False)]:
            self.assertEqual(is_leap(y), want, y)
        self.assertEqual(days_in_week(), 7)
