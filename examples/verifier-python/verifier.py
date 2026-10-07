#!/usr/bin/env python3
"""py-hygiene: an example Anchor verifier plugin (protocol version 1).

Written from docs/verifier/ alone. Standard library only.

For each claim:
  * refuted      if a test the claim cites did not pass, or if a non-test
                 file the claim names gains a leftover debugging hook
                 (breakpoint(), pdb, a FIXME);
  * unverifiable if the input was truncated, or the claim cites no test
                 that ran;
  * supported    otherwise.

Protocol: read one JSON line from stdin, write one JSON line to stdout,
exit 0. Unknown fields are ignored, as the stability rules require.
"""

import json
import re
import sys

NAME = "py-hygiene"
VERSION = "0.1.0"
PROTOCOL = 1

# Added lines that should never reach main.
LEFTOVERS = [
    (re.compile(r"\bbreakpoint\s*\("), "breakpoint()"),
    (re.compile(r"\bimport\s+i?pdb\b|\bi?pdb\.set_trace\s*\("), "pdb"),
    (re.compile(r"#.*\bFIXME\b"), "FIXME"),
]


def verdict(claim_id, verdict, code, refs, note):
    return {
        "claim_id": claim_id,
        "verdict": verdict,
        "reason_code": code,
        "evidence_refs": refs[:64],
        "note": note[:2048],
    }


def judge(claim, payload):
    cid = claim.get("id", "")
    refs = list(payload.get("claim_refs", {}).get(cid, []))
    if payload.get("truncated"):
        return verdict(cid, "unverifiable", "input_truncated", refs,
                       "the input was cut short")

    outcomes = {t.get("id"): t.get("outcome") for t in payload.get("tests", [])}
    cited = [t for t in claim.get("tests", []) if t in outcomes]
    not_passed = [t for t in cited if outcomes[t] != "passed"]
    if not_passed:
        return verdict(cid, "refuted", "cited_test_not_passed",
                       ["test:" + t for t in not_passed],
                       "cited tests did not pass: " + ", ".join(not_passed))

    files = set(claim.get("files", []))
    for change in payload.get("changes", []):
        if change.get("is_test") or change.get("path") not in files:
            continue
        lines = change.get("added_lines", [])
        texts = change.get("added_text", [])
        for line, text in zip(lines, texts):
            for pattern, what in LEFTOVERS:
                if pattern.search(text):
                    path = change["path"]
                    return verdict(cid, "refuted", "debug_leftover",
                                   ["diff:" + path],
                                   "%s:%d adds %s" % (path, line, what))

    if not cited:
        return verdict(cid, "unverifiable", "no_cited_test_ran", refs,
                       "none of the cited tests ran")
    return verdict(cid, "supported", "hygiene_ok", refs,
                   "cited tests passed; no debugging leftovers")


def answer(request):
    rid = request.get("id", 0)
    msg = request.get("msg", {})
    kind = msg.get("kind")
    if request.get("v") != PROTOCOL:
        out = {"kind": "error", "payload": {"code": "unsupported_version"}}
    elif kind == "describe":
        out = {"kind": "info", "payload": {
            "name": NAME, "version": VERSION, "protocol": [PROTOCOL]}}
    elif kind == "verify":
        payload = msg.get("payload", {})
        out = {"kind": "verdicts", "payload": {
            "verdicts": [judge(c, payload) for c in payload.get("claims", [])]}}
    else:
        out = {"kind": "error", "payload": {"code": "unknown_kind"}}
    return {"v": PROTOCOL, "id": rid, "msg": out}


def main():
    line = sys.stdin.readline()
    try:
        request = json.loads(line)
    except ValueError:
        request = {"v": PROTOCOL, "id": 0, "msg": {"kind": "malformed"}}
    sys.stdout.write(json.dumps(answer(request)) + "\n")
    sys.stdout.flush()
    return 0


if __name__ == "__main__":
    sys.exit(main())
