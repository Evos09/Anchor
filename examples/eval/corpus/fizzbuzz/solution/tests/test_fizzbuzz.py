import unittest

from fizz import fizzbuzz


class TargetTest(unittest.TestCase):
    def test_numbers(self):
        self.assertEqual(fizzbuzz(1), "1")
        self.assertEqual(fizzbuzz(7), "7")

    def test_fizz_buzz(self):
        self.assertEqual(fizzbuzz(9), "Fizz")
        self.assertEqual(fizzbuzz(10), "Buzz")
        self.assertEqual(fizzbuzz(30), "FizzBuzz")

    def test_invalid(self):
        with self.assertRaises(ValueError):
            fizzbuzz(0)
