#!/usr/bin/env python3
"""Self-test for event-validate.py (guard rule 19 negative control).

Proves the validator is non-vacuous: a payload missing a required field, a
partial `session.updated`, a bad permission id, and a correct payload. Exit 0
when the validator flags every bad case and accepts the good one.

Usage: event-validate.py --selftest   (delegates here)
"""
import importlib.util
import json
import os
import sys

HERE = os.path.dirname(os.path.abspath(__file__))
spec_mod = importlib.util.spec_from_file_location("ev", os.path.join(HERE, "event-validate.py"))
ev = importlib.util.module_from_spec(spec_mod)
spec_mod.loader.exec_module(ev)

spec = json.load(open(ev.SPEC, encoding="utf-8"))
idx = ev.event_type_index(spec)

FULL_SESSION = {
    "id": "ses_x", "slug": "s", "projectID": "global", "directory": "/w",
    "title": "t", "version": "1", "time": {"created": 1, "updated": 2},
}

cases = [
    # (type, props, expect_valid)
    ("session.updated", {"sessionID": "ses_x", "info": FULL_SESSION}, True),
    # partial session.updated → the 2026-10-09 TUI crash
    ("session.updated", {"sessionID": "ses_x", "info": {"id": "ses_x"}}, False),
    # part.updated missing `time`
    ("message.part.updated",
     {"sessionID": "ses_x", "part": {"id": "prt_x", "sessionID": "ses_x",
      "messageID": "msg_x", "type": "text", "text": "hi"}}, False),
    # part.updated with `time`
    ("message.part.updated",
     {"sessionID": "ses_x", "part": {"id": "prt_x", "sessionID": "ses_x",
      "messageID": "msg_x", "type": "text", "text": "hi"}, "time": 1}, True),
    # permission.asked with an evt_ id (should be per_)
    ("permission.asked",
     {"id": "evt_x", "sessionID": "ses_x", "permission": "edit",
      "patterns": ["*"], "metadata": {}, "always": ["*"]}, False),
    ("permission.asked",
     {"id": "per_abc", "sessionID": "ses_x", "permission": "edit",
      "patterns": ["*"], "metadata": {}, "always": ["*"]}, True),
    # permission.replied missing `reply`
    ("permission.replied", {"sessionID": "ses_x", "requestID": "per_abc"}, False),
    ("permission.replied", {"sessionID": "ses_x", "requestID": "per_abc", "reply": "once"}, True),
    # sync twin suffix resolves to the base type
    ("session.updated.1", {"sessionID": "ses_x", "info": FULL_SESSION}, True),
    # unknown type
    ("totally.made.up", {"x": 1}, False),
]

fails = 0
for etype, props, expect in cases:
    err = ev.validate_payload(props, etype, idx, spec)
    got_valid = err is None
    if got_valid != expect:
        print(f"  SELFTEST FAIL {etype}: expected valid={expect}, err={err!r}")
        fails += 1
    else:
        print(f"  ok {etype} valid={expect}")

if fails:
    sys.exit(f"event-validate selftest: {fails} failing cases")
print("event-validate selftest: all cases pass")
