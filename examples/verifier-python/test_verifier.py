"""Tests of the example plugin against the published fixtures.

Run from the repository root:  python3 -m unittest examples/verifier-python/test_verifier.py
"""

import json
import os
import subprocess
import sys
import unittest

HERE = os.path.dirname(os.path.abspath(__file__))
FIXTURES = os.path.join(HERE, "..", "..", "docs", "verifier", "fixtures")


def ask(request):
    out = subprocess.run(
        [sys.executable, os.path.join(HERE, "verifier.py")],
        input=json.dumps(request) + "\n",
        capture_output=True, text=True, check=True)
    lines = [l for l in out.stdout.splitlines() if l.strip()]
    assert len(lines) == 1, out.stdout
    return json.loads(lines[0])


def verify(name, rid=7):
    with open(os.path.join(FIXTURES, name + ".json")) as f:
        payload = json.load(f)
    r = ask({"v": 1, "id": rid, "msg": {"kind": "verify", "payload": payload}})
    assert r["id"] == rid and r["msg"]["kind"] == "verdicts", r
    return {v["claim_id"]: v for v in r["msg"]["payload"]["verdicts"]}


class T(unittest.TestCase):
    def test_describe(self):
        r = ask({"v": 1, "id": 1, "msg": {"kind": "describe", "payload": {}}})
        self.assertEqual(r["msg"]["kind"], "info")
        self.assertIn(1, r["msg"]["payload"]["protocol"])

    def test_passing_is_supported(self):
        vs = verify("passing")
        self.assertEqual({v["verdict"] for v in vs.values()}, {"supported"})

    def test_failing_test_is_refuted(self):
        self.assertEqual(verify("failing-test")["c1"]["reason_code"],
                         "cited_test_not_passed")

    def test_debug_leftover_is_refuted(self):
        v = verify("debug-leftover")["c1"]
        self.assertEqual(v["verdict"], "refuted")
        self.assertEqual(v["reason_code"], "debug_leftover")

    def test_truncated_is_never_supported(self):
        self.assertEqual(verify("truncated")["c1"]["verdict"], "unverifiable")

    def test_unknown_fields_are_ignored(self):
        with open(os.path.join(FIXTURES, "passing.json")) as f:
            payload = json.load(f)
        payload["x_new"] = {"a": 1}
        r = ask({"v": 1, "id": 3, "x_env": True,
                 "msg": {"kind": "verify", "payload": payload}})
        self.assertEqual(r["msg"]["kind"], "verdicts")


if __name__ == "__main__":
    unittest.main()
