import unittest

from calendar_util import days_in_week


class ExistingTest(unittest.TestCase):
    def test_week(self):
        self.assertEqual(days_in_week(), 7)
