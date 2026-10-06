import unittest

from durations import SECONDS, parse_duration


class HiddenTest(unittest.TestCase):
    def test_all(self):
        self.assertEqual(parse_duration("2h5s"), 7205)
        self.assertEqual(parse_duration("10m"), 600)
        self.assertEqual(parse_duration("0s"), 0)
        for bad in ("", "h", "1m1h", "1h 2m", "1.5h", "s5"):
            with self.assertRaises(ValueError, msg=bad):
                parse_duration(bad)
