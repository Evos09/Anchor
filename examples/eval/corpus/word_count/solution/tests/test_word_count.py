import unittest

from text import word_count


class TargetTest(unittest.TestCase):
    def test_simple(self):
        self.assertEqual(word_count("a b a"), {"a": 2, "b": 1})

    def test_case_and_punctuation(self):
        self.assertEqual(word_count("Go, go GO!"), {"go": 3})
