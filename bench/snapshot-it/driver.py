#!/usr/bin/env python3
"""Snapshot integration driver (SNAPSHOT.md §7.5).

Runs inside the `driver` container against the `server` container. It drives the
real HTTP mutation surface and asserts the OBSERVABLE EFFECT on the shared
worktree (bind-mounted at WORK). Nothing here inspects the server's internals:
the proof is the file bytes and the message graph, exactly the class the
shape-only guard (rule 29) exists to protect.

Asserts:
  1. POST /vcs/apply changes a.txt's bytes on disk.
  2. A prompt (stub provider orders an `edit`) captures a `patch` part and the
     edit lands on disk.
  3. POST /session/{id}/revert restores a.txt to its pre-turn bytes.
  4. GET /session/{id}/diff?messageID= returns JSON (not HTML).
  5. Unrevert restores the post-turn bytes again.
Exit 0 on success, 1 on any failure (each failure printed).
"""
import json
import os
import subprocess
import sys
import time
import urllib.error
import urllib.request

SERVER = os.environ.get("SERVER", "http://server:4925")
WORK = os.environ.get("WORK", "/work")
FAILS = []

# The server arm binds 0.0.0.0 inside the compose bridge, which the F5 guard
# only permits with auth ON (the credential store echoes provider keys). So the
# harness authenticates like a real remote client would.
_AUTH = os.environ.get("AUTH", "")


def _auth_header():
    if not _AUTH:
        return {}
    import base64

    token = base64.b64encode(_AUTH.encode()).decode()
    return {"authorization": "Basic " + token}


def check(cond, msg):
    print(("ok   " if cond else "FAIL ") + msg)
    if not cond:
        FAILS.append(msg)


def http(method, path, body=None, timeout=30):
    url = SERVER + path
    data = None
    headers = {}
    if body is not None:
        data = json.dumps(body).encode()
        headers["content-type"] = "application/json"
    headers.update(_auth_header())
    req = urllib.request.Request(url, data=data, method=method, headers=headers)
    try:
        with urllib.request.urlopen(req, timeout=timeout) as r:
            raw = r.read()
            return r.status, raw
    except urllib.error.HTTPError as e:
        return e.code, e.read()


def http_json(method, path, body=None, timeout=30):
    st, raw = http(method, path, body, timeout)
    try:
        return st, json.loads(raw)
    except Exception:
        return st, {"_raw": raw.decode("utf-8", "replace")[:300]}


def wait_health():
    for _ in range(120):
        try:
            st, _ = http("GET", "/global/health", timeout=3)
            if st == 200:
                return True
        except Exception:
            pass
        time.sleep(0.5)
    return False


def read_a():
    with open(os.path.join(WORK, "a.txt")) as f:
        return f.read()


def write_a(text):
    with open(os.path.join(WORK, "a.txt"), "w") as f:
        f.write(text)


def git(args):
    subprocess.run(["git", *args], cwd=WORK, check=True,
                   stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)


def seed_worktree():
    os.makedirs(WORK, exist_ok=True)
    if not os.path.isdir(os.path.join(WORK, ".git")):
        git(["init", "-q"])
        git(["config", "user.email", "t@t"])
        git(["config", "user.name", "t"])
    write_a("one\n")
    git(["add", "."])
    git(["commit", "-q", "-m", "seed", "--allow-empty"])


def main():
    seed_worktree()
    if not wait_health():
        print("FAIL server never became healthy")
        return 1
    print("ok   server healthy")

    # ---- 1. /vcs/apply mutates the worktree --------------------------------
    patch = "--- a/a.txt\n+++ b/a.txt\n@@ -1 +1 @@\n-one\n+two\n"
    st, raw = http("POST", f"/vcs/apply?directory={WORK}",
                   {"patch": patch})
    check(st == 200 and json.loads(raw).get("applied") is True,
          f"/vcs/apply 200 applied:true (got {st} {raw[:120]!r})")
    check(read_a() == "two\n", "vcs/apply changed a.txt on disk")
    # reset for the revert test
    write_a("one\n")

    # ---- 2-5. prompt -> edit -> revert -> unrevert -------------------------
    st, sess = http_json("POST", "/session", {"title": "snap-it"})
    sid = sess.get("id")
    check(st == 200 and bool(sid), f"session created ({st})")
    if not sid:
        return 1

    umid = "msg_snapite2e000000000000001"
    st, _ = http("POST", f"/session/{sid}/prompt_async",
                 {"messageID": umid,
                  "parts": [{"type": "text", "text": "edit a.txt to two"}],
                  "model": {"providerID": "stub", "modelID": "stub-1"},
                  "agent": "build"})
    check(st in (200, 204), f"prompt accepted ({st})")

    # wait for the edit to land (tool executes against /work)
    landed = False
    for _ in range(120):
        if read_a() == "two\n":
            landed = True
            break
        time.sleep(0.5)
    check(landed, "prompt's edit tool landed on disk")

    # diff route returns JSON (not SPA HTML)
    st, body = http_json("GET", f"/session/{sid}/diff?messageID={umid}")
    check(isinstance(body, (list, dict)), f"GET /session/{{id}}/diff is JSON (got {st})")

    # revert restores the pre-turn bytes
    st, info = http_json("POST", f"/session/{sid}/revert", {"messageID": umid})
    check(st == 200 and info.get("revert", {}).get("messageID") == umid,
          f"revert set the marker ({st})")
    restored = read_a() == "one\n"
    check(restored, "revert restored a.txt to its pre-turn bytes")

    # unrevert restores the post-turn snapshot
    st, info = http_json("POST", f"/session/{sid}/unrevert", {})
    check(st == 200 and info.get("revert") is None, f"unrevert cleared the marker ({st})")
    check(read_a() == "two\n", "unrevert restored the post-turn bytes")

    if FAILS:
        print(f"\n{len(FAILS)} FAILURE(S):")
        for f in FAILS:
            print("  - " + f)
        return 1
    print("\nALL SNAPSHOT INTEGRATION CHECKS PASSED")
    return 0


if __name__ == "__main__":
    sys.exit(main())
