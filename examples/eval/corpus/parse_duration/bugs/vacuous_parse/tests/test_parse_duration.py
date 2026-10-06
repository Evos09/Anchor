import unittest

from durations import parse_duration


class TargetTest(unittest.TestCase):
    def test_parse(self):
        self.assertGreater(parse_duration("1h30m"), 0)
        self.assertGreater(parse_duration("45s"), 0)

    def test_invalid(self):
        with self.assertRaises(ValueError):
            parse_duration("")
        with self.assertRaises(ValueError):
            parse_duration("5x")
