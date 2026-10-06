import unittest

from text import shout


class ExistingTest(unittest.TestCase):
    def test_shout(self):
        self.assertEqual(shout('hi'), 'HI!')
