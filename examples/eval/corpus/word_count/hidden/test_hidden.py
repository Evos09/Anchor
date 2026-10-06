import unittest

from text import shout, word_count


class HiddenTest(unittest.TestCase):
    def test_all(self):
        self.assertEqual(word_count(""), {})
        self.assertEqual(word_count("don't stop, DON'T"), {"don't": 2, "stop": 1})
        self.assertEqual(word_count("route 66 and 66"), {"route": 1, "66": 2, "and": 1})
        self.assertEqual(word_count("tab\tsep\nline"), {"tab": 1, "sep": 1, "line": 1})
