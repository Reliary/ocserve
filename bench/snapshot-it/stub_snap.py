#!/usr/bin/env python3
"""Snapshot-it stub provider: an OpenAI-compatible SSE endpoint whose FIRST
response is an `edit` tool call (a.txt one->two) and whose SECOND is a plain
stop. This makes ocserve exercise its real tool loop + snapshot patch capture
through HTTP, exactly as a live model would.

Env: STUB_PORT (default 8080).
"""
import json
import os
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

PORT = int(os.environ.get("STUB_PORT", "8080"))
STATE = {"n": 0}


def sse(obj):
    return ("data: " + json.dumps(obj) + "\n\n").encode()


class H(BaseHTTPRequestHandler):
    def log_message(self, *a):
        pass

    def do_POST(self):
        n = int(self.headers.get("content-length", "0"))
        _ = self.rfile.read(n)
        STATE["n"] += 1
        first = STATE["n"] == 1
        self.send_response(200)
        self.send_header("content-type", "text/event-stream")
        self.send_header("cache-control", "no-cache")
        self.send_header("connection", "close")
        self.end_headers()
        if first:
            chunk = {
                "id": "chatcmpl-snap-1",
                "object": "chat.completion.chunk",
                "choices": [{
                    "index": 0,
                    "delta": {"tool_calls": [{
                        "index": 0,
                        "id": "call_snap_edit",
                        "type": "function",
                        "function": {
                            "name": "edit",
                            "arguments": json.dumps({
                                "filePath": "a.txt",
                                "oldString": "one",
                                "newString": "two",
                            }),
                        },
                    }]},
                }],
            }
            self.wfile.write(sse(chunk))
            self.wfile.write(sse({"id": "chatcmpl-snap-1", "object": "chat.completion.chunk",
                                  "choices": [{"index": 0, "delta": {}, "finish_reason": "tool_calls"}]}))
            self.wfile.write(b"data: [DONE]\n\n")
        else:
            self.wfile.write(sse({"id": "chatcmpl-snap-2", "object": "chat.completion.chunk",
                                  "choices": [{"index": 0, "delta": {"content": "done"}, "finish_reason": None}]}))
            self.wfile.write(sse({"id": "chatcmpl-snap-2", "object": "chat.completion.chunk",
                                  "choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}],
                                  "usage": {"prompt_tokens": 10, "completion_tokens": 2, "total_tokens": 12}}))
            self.wfile.write(b"data: [DONE]\n\n")


if __name__ == "__main__":
    ThreadingHTTPServer(("0.0.0.0", PORT), H).serve_forever()
