import unittest

from arith import square


class ExistingTest(unittest.TestCase):
    def test_square(self):
        self.assertEqual(square(3), 9)
