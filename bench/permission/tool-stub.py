#!/usr/bin/env python3
"""Deterministic OpenAI-compatible stub that emits the tool call named by the
last user message.

Reuses the capdiff rule idea but keys on the tool-case matrix directly: the
prompt text is a case name, so the emitted tool_call is a pure function of the
case, identical on both arms. One tool round per turn, then a fixed text reply.

  POST /v1/chat/completions   (streaming SSE)
  GET  /_requests             -> {"n": <requests seen>}
"""
import hashlib
import json
import sys
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

CASES = {}
# Set from argv[3]; substituted for "$WORK" in tool arguments.
WORK: str | None = None


def subst(value):
    if isinstance(value, str):
        work = globals().get("WORK")
        return value.replace("$WORK", work) if work else value
    if isinstance(value, list):
        return [subst(v) for v in value]
    if isinstance(value, dict):
        return {k: subst(v) for k, v in value.items()}
    return value


def last_user_text(messages):
    for m in reversed(messages or []):
        if m.get("role") != "user":
            continue
        c = m.get("content")
        if isinstance(c, str):
            return c
        if isinstance(c, list):
            return "".join(p.get("text") or "" for p in c if isinstance(p, dict))
    return ""


def already_used_tool(messages):
    for m in messages or []:
        if m.get("role") in ("tool", "function"):
            return True
        if m.get("role") == "assistant" and m.get("tool_calls"):
            return True
    return False


class H(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, *a):
        pass

    def _json(self, obj, status=200):
        body = json.dumps(obj).encode()
        self.send_response(status)
        self.send_header("content-type", "application/json")
        self.send_header("content-length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def do_GET(self):
        if self.path.startswith("/_requests"):
            self._json({"n": H.seen})
            return
        self._json({"ok": True})

    def do_POST(self):
        n = int(self.headers.get("content-length", 0) or 0)
        raw = self.rfile.read(n)
        try:
            body = json.loads(raw)
        except Exception:
            body = {}
        H.seen += 1
        messages = body.get("messages") or []
        text = last_user_text(messages)

        name = None
        for case_name in CASES:
            if case_name in text:
                name = case_name
                break

        if name and not already_used_tool(messages):
            case = CASES[name]
            frames = [
                {"choices": [{"index": 0, "delta": {"role": "assistant", "tool_calls": [{
                    "index": 0,
                    "id": "call_permoracle",
                    "type": "function",
                    "function": {"name": case["tool"],
                                 "arguments": json.dumps(subst(case["arguments"]))},
                }]}}]},
                {"choices": [{"index": 0, "delta": {}, "finish_reason": "tool_calls"}]},
                {"choices": [], "usage": {"prompt_tokens": 11, "completion_tokens": 3,
                                          "total_tokens": 14}},
            ]
        else:
            digest = hashlib.sha256(text.encode()).hexdigest()[:12]
            frames = [
                {"choices": [{"index": 0, "delta": {"role": "assistant", "content": "answer-"}}]},
                {"choices": [{"index": 0, "delta": {"content": digest}}]},
                {"choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}]},
                {"choices": [], "usage": {"prompt_tokens": 11, "completion_tokens": 3,
                                          "total_tokens": 14}},
            ]

        self.send_response(200)
        self.send_header("content-type", "text/event-stream")
        self.send_header("cache-control", "no-cache")
        self.send_header("connection", "close")
        self.end_headers()
        for f in frames:
            self.wfile.write(f"data: {json.dumps(f)}\n\n".encode())
        self.wfile.write(b"data: [DONE]\n\n")
        self.wfile.flush()
        self.close_connection = True


H.seen = 0

if __name__ == "__main__":
    port, cases_file = int(sys.argv[1]), sys.argv[2]
    doc = json.load(open(cases_file))
    CASES.update({c["name"]: c for c in doc["cases"]})
    globals()["WORK"] = sys.argv[3] if len(sys.argv) > 3 else None
    ThreadingHTTPServer(("127.0.0.1", port), H).serve_forever()
