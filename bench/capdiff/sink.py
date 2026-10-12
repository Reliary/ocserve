#!/usr/bin/env python3
"""Standalone OpenAI-compatible capture sink (used by
scripts/capture-provider-contract.sh). Records each request body to
<outdir>/req-<ns>.json and replies with a deterministic SSE completion."""
import http.server, json, pathlib, socketserver, sys, threading, time

port, outdir = int(sys.argv[1]), pathlib.Path(sys.argv[2])
outdir.mkdir(parents=True, exist_ok=True)

class H(http.server.BaseHTTPRequestHandler):
    def log_message(self, *a): pass
    def do_POST(self):
        n = int(self.headers.get("content-length", 0) or 0)
        raw = self.rfile.read(n)
        try: body = json.loads(raw)
        except Exception: body = {"_raw": raw[:500].decode("utf-8", "replace")}
        (outdir / f"req-{time.time_ns()}.json").write_text(json.dumps({
            "path": self.path,
            "headers": {k.lower(): v for k, v in self.headers.items()},
            "body": body}, indent=2))
        self.send_response(200)
        self.send_header("content-type", "text/event-stream")
        self.end_headers()
        for c in [{"choices":[{"delta":{"content":"STUB"}}]},
                  {"choices":[{"delta":{},"finish_reason":"stop"}]},
                  {"choices":[],"usage":{"prompt_tokens":1,"completion_tokens":1,"total_tokens":2}}]:
            self.wfile.write(f"data: {json.dumps(c)}\n\n".encode())
        self.wfile.write(b"data: [DONE]\n\n"); self.wfile.flush()

class S(socketserver.ThreadingTCPServer):
    allow_reuse_address = True
    daemon_threads = True
S(("127.0.0.1", port), H).serve_forever()
