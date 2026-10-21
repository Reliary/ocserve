#!/usr/bin/env python3
"""Install each corpus package's dependencies (and its peer deps).

Rules, all load-bearing:

* `--ignore-scripts` — a postinstall hook is arbitrary code, and this runs while
  the network is up. The sandbox stage (network off) is where plugin code runs.
* Peer deps are installed explicitly — npm does not, and a peer-only plugin
  would otherwise fail to resolve `@opencode-ai/plugin` and be reported as
  incompatible when the real cause is our harness.
* Peers are pinned to the SDK versions the server implements (1.18.31 v1,
  2.0.28 v2). A declared range would resolve to a newer SDK than ocserve
  speaks, which measures the wrong contract.
* Bun first, because OpenCode itself installs plugins with Bun and its resolver
  is what a real install looks like. npm is the fallback.
* Every install is bounded. A dependency that cannot install in the budget is
  reported as missing-dependency, which is an honest result, not a hang.

Exit 0 even when some packages fail: a package with a broken or unpublished
dependency is a result, not a harness failure.
"""
import json
import os
import shutil
import subprocess
import sys

BUDGET = int(os.environ.get("PLUGIN_DEP_BUDGET", "180"))
PIN = {"@opencode-ai/plugin": "1.18.31", "@opencode/plugin": "2.0.28"}
LOCKS = ("package-lock.json", "bun.lock", "bun.lockb", "yarn.lock")


def which_bun():
    p = shutil.which("bun")
    if p:
        return p
    local = os.path.expanduser("~/.bun/bin/bun")
    return local if os.path.exists(local) else None


def run(cmd, cwd):
    try:
        r = subprocess.run(cmd, cwd=cwd, capture_output=True, text=True, timeout=BUDGET)
        return r.returncode, r.stderr
    except subprocess.TimeoutExpired:
        return 124, f"timeout after {BUDGET}s"


def main():
    corpus = sys.argv[1]
    bun, npm = which_bun(), shutil.which("npm")
    if not bun and not npm:
        print("deps: no bun or npm; plugins with runtime deps report missing-dependency")
        return
    ok = failed = 0
    for name in sorted(os.listdir(corpus)):
        d = os.path.join(corpus, name)
        pj = os.path.join(d, "package.json")
        if not os.path.isfile(pj):
            continue
        try:
            pkg = json.load(open(pj, encoding="utf-8"))
        except Exception:
            failed += 1
            continue
        # Clear lockfiles: a lock from a previous stage puts Bun in frozen mode,
        # and the manifest pin is the real lock here.
        for lock in LOCKS:
            try:
                os.remove(os.path.join(d, lock))
            except FileNotFoundError:
                pass
        deps = dict(pkg.get("dependencies") or {})
        peers = {k: PIN.get(k, v) for k, v in (pkg.get("peerDependencies") or {}).items()}
        merged = dict(pkg)
        merged["dependencies"] = {**deps, **peers}
        with open(pj, "w", encoding="utf-8") as fh:
            json.dump(merged, fh, indent=2)

        rc, err = (124, "no runtime")
        if bun:
            rc, err = run([bun, "install", "--ignore-scripts", "--no-summary", "--production"], d)
        if rc != 0:
            # Retry with the package's own deps only: an unsatisfiable peer must
            # not also remove the deps that make the plugin loadable.
            with open(pj, "w", encoding="utf-8") as fh:
                json.dump(pkg, fh, indent=2)
            if bun:
                rc, err = run([bun, "install", "--ignore-scripts", "--no-summary", "--production"], d)
            if rc != 0 and npm:
                cmd = [
                    npm, "install", "--prefix", d, "--omit=dev", "--ignore-scripts",
                    "--no-audit", "--no-fund", "--loglevel=error", "--legacy-peer-deps",
                ] + [f"{k}@{v}" for k, v in {**deps, **peers}.items()]
                rc, err = run(cmd, None)
        if rc == 0:
            ok += 1
        else:
            failed += 1
            line = (err or "").strip().splitlines()
            print(f"dep-fail {name}: {line[-1][:110] if line else 'unknown'}")
    print(f"deps: {ok} installed, {failed} failed (failures report as missing-dependency)")


if __name__ == "__main__":
    main()
