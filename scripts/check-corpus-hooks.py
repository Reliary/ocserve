#!/usr/bin/env python3
"""Rule 48: the real-plugin corpus must invoke hooks that actually exist.

`bench/plugins/check.mjs` shipped `"chat.messages.transform"` — a name that is in
neither the SDK's `Hooks` interface nor any plugin. `V1_HOOKS.includes(name)` was
therefore always false for the real `experimental.chat.messages.transform`, and
the corpus reported `hooks-ran` without ever exercising the hook. That is the
"test that cannot fail" class.

Source of truth: bench/hooks/freeze.surface.json, captured from the reference
implementation by scripts/capture-hooks.sh.
"""
import json
import pathlib
import re
import sys

ROOT = pathlib.Path(__file__).resolve().parent.parent
SURFACE = ROOT / "bench/hooks/freeze.surface.json"
CORPUS = ROOT / "bench/plugins/check.mjs"


def main() -> int:
    if not SURFACE.exists():
        print(f"missing {SURFACE.relative_to(ROOT)}; run scripts/capture-hooks.sh")
        return 1
    real = set(json.loads(SURFACE.read_text())["surface"])
    src = CORPUS.read_text()
    block = re.search(r"const V1_HOOKS = \[(.*?)\]", src, re.S)
    if not block:
        print("check.mjs: V1_HOOKS not found")
        return 1
    names = re.findall(r'"([^"]+)"', block.group(1))
    bad = [n for n in names if n not in real]
    if bad:
        print("not a real hook:", ", ".join(bad))
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
