import unittest

from lists import first, flatten


class HiddenTest(unittest.TestCase):
    def test_all(self):
        self.assertEqual(flatten([]), [])
        self.assertEqual(flatten([[], [[]]]), [])
        self.assertEqual(flatten([[1, [2]], [[3]], 4]), [1, 2, 3, 4])
        self.assertEqual(flatten(["ab", ["c"]]), ["ab", "c"])
        self.assertEqual(flatten([(1, 2), [3]]), [(1, 2), 3])
        deep = [1]
        for _ in range(12):
            deep = [deep]
        self.assertEqual(flatten(deep), [1])
