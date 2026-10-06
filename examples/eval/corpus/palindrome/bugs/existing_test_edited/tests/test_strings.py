import unittest

from strings import reverse


class ExistingTest(unittest.TestCase):
    def test_reverse(self):
        self.assertEqual(reverse('aba'), 'aba')
