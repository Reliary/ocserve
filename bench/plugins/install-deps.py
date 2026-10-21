#!/usr/bin/env python3
"""Install each corpus package's dependencies (and its peer deps).

Two rules, both load-bearing:

* `--ignore-scripts` — a postinstall hook is arbitrary code, and this runs while
  the network is up (the execution stage is the sandbox, not this one).
* peer deps are installed explicitly — npm does not, and a peer-only plugin
  would otherwise fail to resolve `@opencode-ai/plugin` and be reported as
  incompatible when the real cause is our harness.

Exit 0 even when some packages fail: a package with a broken or unpublished
dependency is a result ("load-error: dependency"), not a harness failure.
"""
import json
import os
import shutil
import subprocess
import sys

corpus = sys.argv[1]
# Bun is what OpenCode itself uses to install plugins, and its resolver/hoisting
# is what a real install looks like. npm's stricter tree produces failures that
# no user would see. Fall back to npm only when bun is absent.
BUN = shutil.which("bun") or (os.path.expanduser("~/.bun/bin/bun") if os.path.exists(os.path.expanduser("~/.bun/bin/bun")) else None)
# The SDK versions the server actually speaks. Using the range the plugin
# declares picks a newer SDK than ocserve implements, which measures the wrong
# thing; pin to the contract.
PIN = {
    "@opencode-ai/plugin": "1.18.31",
    "@opencode/plugin": "2.0.28",
}
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
    peers = [
        f"{k}@{PIN.get(k, v)}" for k, v in (pkg.get("peerDependencies") or {}).items()
    ]
    # The package's own deps are passed in the SAME command as the peers. Two
    # sequential `npm install` calls do not work: the second prunes the peers
    # installed by the first as extraneous, which produced a corpus full of
    # false "Cannot find package @opencode-ai/plugin" load errors.
    own = [f"{k}@{v}" for k, v in (pkg.get("dependencies") or {}).items()]
    if BUN:
        # An npm lockfile from a previous fetch stage makes Bun enter frozen
        # mode and abort. Remove it: the corpus is ephemeral and the manifest
        # pin is the real lock.
        for lock in ("package-lock.json", "bun.lock", "bun.lockb", "yarn.lock"):
            try:
                os.remove(os.path.join(d, lock))
            except FileNotFoundError:
                pass
        base = [BUN, "install", "--ignore-scripts", "--no-summary", "--production"]
        cwd = d
    else:
        base = [
            "npm", "install", "--prefix", d, "--omit=dev", "--ignore-scripts",
            "--no-audit", "--no-fund", "--loglevel=error", "--legacy-peer-deps",
        ]
        cwd = None
    # Peers first, then the package's own deps. Two calls because npm treats an
    # explicit peer spec as a normal dependency and we want the pinned version
    # to win over whatever the package's ranges would resolve to.
    def run(extra):
        if BUN:
            # Bun installs declared deps from package.json; explicit specs are
            # written into the package first so peer pins are honoured.
            return subprocess.run(base + extra, capture_output=True, text=True, cwd=cwd)
        return subprocess.run(base + extra, capture_output=True, text=True, cwd=cwd)

    if BUN:
        # Materialise the pins in package.json so bun resolves them.
        pj2 = json.load(open(pj, encoding="utf-8"))
        deps = dict(pj2.get("dependencies") or {})
        for spec in peers:
            k, _, v = spec.partition("@") if not spec.startswith("@") else (spec.rsplit("@", 1)[0], "", spec.rsplit("@", 1)[1])
            deps[k] = v
        merged = dict(pj2)
        merged["dependencies"] = deps
        with open(pj, "w", encoding="utf-8") as fh:
            json.dump(merged, fh, indent=2)
        r = run([])
        if r.returncode != 0:
            # Restore and retry without the peer additions.
            with open(pj, "w", encoding="utf-8") as fh:
                json.dump(pj2, fh, indent=2)
            r = run([])
    else:
        r = run(own + peers)
        if r.returncode != 0:
            # Some peer ranges are unresolvable by this npm (an optional peer
            # that was never published at a satisfying version). Retry with the
            # package's own deps only; the report then says "missing-dependency".
            r = run(own)
    if r.returncode == 0:
        ok += 1
    else:
        failed += 1
        reason = (r.stderr or "").strip().splitlines()
        print(f"dep-fail {name}: {reason[-1][:120] if reason else 'unknown'}")
print(f"deps: {ok} installed, {failed} failed (failures stay load-errors in the report)")
