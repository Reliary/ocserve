#!/usr/bin/env python3
"""Deterministic OpenAI-compatible stub for the parity harness.

Design (antagonism record):
- The reply is a pure function of the LAST user message only — normalize
  string content and `[{type:text,...}]` parts to one string, hash it, and
  derive both wording and length from the hash. Upstream and refine send
  different system prompts, tool schemas and message wrappers; as long as
  the user text is identical, the completion is byte-identical across
  arms, so seeded histories stay logically equal.
- Paced streaming (STUB_TOK_PER_SEC, default 120): first chunk is sent
  immediately (fair TTFB), remaining chunks sleep to simulate a real
  provider's generation speed without any provider-side variance.
- tools are accepted and ignored — never any tool_calls, so both arms'
  tool loops exit after one step.
- GET /_stats → request count for smoke assertions.
"""
import hashlib
import json
import os
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

TOK_PER_SEC = float(os.environ.get("STUB_TOK_PER_SEC", "120"))
MODEL = os.environ.get("STUB_MODEL", "stub-1")
POOL = (
    "the index confirms the requested structure and the pipeline state "
    "remains consistent across the inspected regions reporting measured "
    "values with provenance and no unverified claims next steps are listed "
    "in order of priority with owners attached where known tests stay green "
    "and guards pass so the change is safe to keep building on further "
).split()

N_REQUESTS = 0
LAST = {}


def user_text(messages):
    """Last user message only; string or content-part list; str(bytes)."""
    for m in reversed(messages or []):
        if m.get("role") != "user":
            continue
        c = m.get("content")
        if isinstance(c, str):
            return c
        if isinstance(c, list):
            out = []
            for part in c:
                if isinstance(part, dict) and part.get("type") == "text":
                    out.append(part.get("text") or "")
                elif isinstance(part, str):
                    out.append(part)
            return "\n".join(out)
        if c is None:
            return ""
        return str(c)
    return ""


def derive(text):
    digest = hashlib.sha256(text.encode("utf-8", "replace")).digest()
    force = os.environ.get("STUB_FORCE_WORDS", "")
    n_words = int(force) if force.isdigit() else 90 + digest[0]  # 90..145 words
    words = [POOL[digest[i % len(digest)] % len(POOL)] for i in range(n_words)]
    # embed the short hash so logs/smoke can cross-check both arms
    words[0] = digest.hex()[:8]
    return " ".join(words)


class Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, fmt, *args):  # quiet; we emit our own line
        pass

    def _json(self, code, obj):
        body = json.dumps(obj).encode()
        self.send_response(code)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def do_GET(self):
        if self.path.startswith("/v1/models"):
            self._json(200, {
                "object": "list",
                "data": [{
                    "id": MODEL, "object": "model",
                    "created": 0, "owned_by": "parity",
                }],
            })
        elif self.path.startswith("/_stats"):
            self._json(200, {"n": N_REQUESTS, "last": LAST})
        else:
            self._json(404, {"error": {"message": f"not found: {self.path}"}})

    def do_POST(self):
        global N_REQUESTS
        t0 = time.monotonic()
        if not self.path.startswith("/v1/chat/completions"):
            self._json(404, {"error": {"message": f"not found: {self.path}"}})
            return
        try:
            n = int(self.headers.get("Content-Length") or 0)
            req = json.loads(self.rfile.read(n) or b"{}")
        except Exception:
            self._json(400, {"error": {"message": "bad json"}})
            return
        messages = req.get("messages") or []
        text = derive(user_text(messages))
        model = req.get("model") or MODEL
        created = int(time.time())
        prompt_chars = sum(
            len(json.dumps(m.get("content") or "")) for m in messages
        )
        usage = {
            "prompt_tokens": max(1, prompt_chars // 4),
            "completion_tokens": len(text.split()),
            "total_tokens": max(1, prompt_chars // 4) + len(text.split()),
        }
        N_REQUESTS += 1
        LAST = {"model": model, "stream": bool(req.get("stream")),
                "n_messages": len(messages), "ms": int((time.monotonic() - t0) * 1000)}
        print(f"req #{N_REQUESTS} stream={bool(req.get('stream'))} "
              f"n_msg={len(messages)} model={model} sha={hashlib.sha256(text.encode()).hexdigest()[:8]}",
              flush=True)
        if req.get("stream"):
            self._stream(model, text, created, usage, t0)
        else:
            self._json(200, {
                "id": f"chatcmpl-parity", "object": "chat.completion",
                "created": created, "model": model,
                "choices": [{"index": 0,
                             "message": {"role": "assistant", "content": text},
                             "finish_reason": "stop"}],
                "usage": usage,
            })

    def _stream(self, model, text, created, usage, t0):
        # M1 antagonism fix: an SSE body with NO Content-Length and NO
        # Transfer-Encoding is close-delimited in HTTP/1.1 — the client
        # waits for EOF that never comes (upstream /message hung >120s,
        # then reset with empty assistant text). Chunk every frame.
        self.send_response(200)
        self.send_header("Content-Type", "text/event-stream")
        self.send_header("Cache-Control", "no-cache")
        self.send_header("Connection", "keep-alive")
        self.send_header("Transfer-Encoding", "chunked")
        self.end_headers()
        words = text.split()
        chunk_words = 6
        delay = chunk_words / max(TOK_PER_SEC, 1.0)
        idx, i = 0, 0
        first = True
        while i < len(words):
            piece = " ".join(words[i:i + chunk_words])
            if not piece.endswith(" "):
                piece += " " if i + chunk_words < len(words) else ""
            i += chunk_words
            if not first:
                time.sleep(delay)
            first = False
            self._sse({
                "id": f"chatcmpl-parity-{created}", "object": "chat.completion.chunk",
                "created": created, "model": model,
                "choices": [{"index": 0, "delta": {"content": piece},
                             "finish_reason": None}],
            })
        self._sse({
            "id": f"chatcmpl-parity-{created}", "object": "chat.completion.chunk",
            "created": created, "model": model,
            "choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}],
            "usage": usage,
        })
        self._raw(b"data: [DONE]\n\n")
        # terminator is NOT a chunk — writing it through _raw would
        # length-prefix it (5\r\n0\r\n\r\n) and the client would
        # wait for a real 0-chunk forever
        try:
            self.wfile.write(b"0\r\n\r\n")
        except (BrokenPipeError, ConnectionResetError):
            self.close_connection = True
            return
        try:
            self.wfile.flush()
        except (BrokenPipeError, ConnectionResetError):
            pass

    def _sse(self, obj):
        self._raw(b"data: " + json.dumps(obj).encode() + b"\n\n")

    def _raw(self, b):
        # every frame is one HTTP chunk (length-prefixed); a client that
        # disconnects mid-stream must not kill the handler thread
        try:
            self.wfile.write(f"{len(b):X}\r\n".encode() + b + b"\r\n")
        except (BrokenPipeError, ConnectionResetError):
            self.close_connection = True
            raise


def main():
    port = int(os.environ.get("STUB_PORT", "8080"))
    srv = ThreadingHTTPServer(("0.0.0.0", port), Handler)
    srv.daemon_threads = True
    print(f"parity stub on :{port} tok/s={TOK_PER_SEC} model={MODEL}", flush=True)
    srv.serve_forever()


if __name__ == "__main__":
    main()
