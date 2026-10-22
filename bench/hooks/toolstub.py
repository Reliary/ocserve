#!/usr/bin/env python3
"""Deterministic OpenAI-compatible stub that emits ONE tool call, then text.

Why a second stub: the parity stub never emits `tool_calls`, so a prompt
scenario can never reach `tool.execute.before/after` or the bash tool's
`shell.env`. This one does exactly one round-trip per user turn and is
otherwise inert (same reply for the same last-user-message hash), so both arms
see identical content.

  POST /v1/chat/completions   (streaming SSE)
  GET  /_requests             -> number of requests seen (smoke assertion)
"""
import hashlib
import json
import os
import threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

LOCK = threading.Lock()
SEEN = {"n": 0}
MODEL = os.environ.get("STUB_MODEL", "stub-1")


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
    """A tool result in the transcript means the tool round already happened."""
    for m in messages or []:
        if m.get("role") in ("tool", "function"):
            return True
    return False


class H(BaseHTTPRequestHandler):
    # HTTP/1.0 + connection-close framing: the SSE body is terminated by EOF,
    # which is what the capdiff sink does and what the provider clients accept.
    # (An HTTP/1.1 response without content-length or chunked framing makes the
    # client wait forever — measured.)
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
            self._json(SEEN)
            return
        self._json({"ok": True})

    def do_POST(self):
        n = int(self.headers.get("content-length", 0) or 0)
        raw = self.rfile.read(n)
        try:
            body = json.loads(raw)
        except Exception:
            body = {}
        with LOCK:
            SEEN["n"] += 1
        messages = body.get("messages") or []
        text = last_user_text(messages)
        digest = hashlib.sha256(text.encode()).hexdigest()[:12]

        if not already_used_tool(messages):
            args = {"command": f"echo hook-tool-{digest}"}
            chunks = [
                {
                    "choices": [
                        {
                            "index": 0,
                            "delta": {
                                "role": "assistant",
                                "tool_calls": [
                                    {
                                        "index": 0,
                                        "id": "call_hookprobe",
                                        "type": "function",
                                        "function": {
                                            "name": "bash",
                                            "arguments": json.dumps(args),
                                        },
                                    }
                                ],
                            },
                        }
                    ]
                },
                {
                    "choices": [
                        {
                            "index": 0,
                            "delta": {},
                            "finish_reason": "tool_calls",
                        }
                    ]
                },
            ]
        else:
            chunks = [
                {"choices": [{"index": 0, "delta": {"role": "assistant", "content": "answer-"}}]},
                {"choices": [{"index": 0, "delta": {"content": digest}}]},
                {"choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}]},
            ]

        self.send_response(200)
        self.send_header("content-type", "text/event-stream")
        self.send_header("cache-control", "no-cache")
        self.send_header("connection", "close")
        self.end_headers()
        for c in chunks:
            self.wfile.write(("data: " + json.dumps(c) + "\n\n").encode())
        self.wfile.write(b"data: [DONE]\n\n")
        try:
            self.wfile.flush()
        except Exception:
            pass
        self.close_connection = True


def main():
    port = int(os.environ.get("STUB_PORT", "18098"))
    srv = ThreadingHTTPServer(("127.0.0.1", port), H)
    srv.daemon_threads = True
    print(f"hook stub on :{port}", flush=True)
    srv.serve_forever()


if __name__ == "__main__":
    main()
