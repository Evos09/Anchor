import unittest

from fizz import is_multiple, fizzbuzz


class HiddenTest(unittest.TestCase):
    def test_all(self):
        got = [fizzbuzz(i) for i in range(1, 16)]
        self.assertEqual(got, ["1", "2", "Fizz", "4", "Buzz", "Fizz", "7", "8",
                               "Fizz", "Buzz", "11", "Fizz", "13", "14", "FizzBuzz"])
        self.assertEqual(fizzbuzz(45), "FizzBuzz")
        self.assertEqual(fizzbuzz(25), "Buzz")
        with self.assertRaises(ValueError):
            fizzbuzz(-3)
        self.assertTrue(is_multiple(10, 5))
