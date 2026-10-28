#!/usr/bin/env python3
"""Rule 61: the gate receipts are both WRITTEN and READ.

Why this exists. `scripts/nightly.sh` exited non-zero on 19 of 21 runs between
2026-10-04 and 2026-10-27. Nothing surfaced it: the timer's exit code lived
only in `systemctl --user show`, the log in a gitignored directory, and
`ocserve doctor` did not know the gate existed. The receipts added in response
are only worth anything if:

  1. every scheduled verification program actually writes one, and
  2. `doctor` actually fails when a receipt says the gate is unhealthy.

Either half alone is the registered-but-never-called class this repo has
already paid for twice (the v1 `config` plugin hook, `fetch-release.sh
--selftest`). A receipt nobody writes is silent; a reader nobody reaches is
decoration.

This is the `--selftest` form required by rule 39: it plants each half of the
failure and refuses to report ok unless the predicate catches it.
"""

import re
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent

# (script, receipt call that must appear, gate name it must name)
WRITERS = [
    ("scripts/nightly.sh", "gate_begin", "nightly"),
    ("scripts/nightly.sh", "gate_finish", "nightly"),
    ("scripts/drift-watch.sh", "gate_begin", "drift-watch"),
    ("scripts/drift-watch.sh", "gate_finish", "drift-watch"),
    ("scripts/soak.sh", "gate_sampler", "soak"),
]

READER_FN = "gate_status::report"
READER_FAIL = "gates_ok"


def written(script_text: str, call: str, gate: str) -> bool:
    """The call is present AND names the gate it belongs to.

    Naming matters: `. gate-status.sh` at the top of a script proves nothing,
    and a copy-paste that calls `gate_begin nightly` from the soak script would
    write the wrong receipt (doctor would then report the soak as stale while
    nightly looked fresh twice over).
    """
    for line in script_text.splitlines():
        stripped = line.strip()
        if stripped.startswith("#"):
            continue
        if re.search(rf"\b{call}\b", stripped) and re.search(
            rf"\b{re.escape(gate)}\b", stripped
        ):
            return True
    return False


def _has_source(script_text: str) -> bool:
    """The script sources the receipt library (a non-comment `. path` line)."""
    for line in script_text.splitlines():
        stripped = line.strip()
        if stripped.startswith("#") or "gate-status.sh" not in stripped:
            continue
        if stripped.startswith(".") or stripped.startswith("source "):
            return True
    return False


def reader_wired(main_rs: str) -> bool:
    """`doctor` computes the verdict and can fail on it.

    Both halves are required: computing it and ignoring the result is the
    checks-that-never-fail pattern.
    """
    if READER_FN not in main_rs:
        return False
    # The verdict must reach a failure path in the doctor body.
    if not re.search(rf"\b{READER_FAIL}\b\s*=\s*{re.escape(READER_FN)}", main_rs):
        return False
    # ...and be consumed by a bail/exit.
    tail = main_rs.split(f"{READER_FAIL} = {READER_FN}", 1)[1]
    return bool(re.search(rf"if\s+!\s*{READER_FAIL}\b", tail)) and bool(
        re.search(r"(anyhow::bail!|process::exit\()", tail)
    )


def check(root: Path) -> list[str]:
    problems: list[str] = []
    texts: dict[str, str] = {}
    for script, call, gate in WRITERS:
        p = root / script
        if not p.exists():
            problems.append(f"{script}: missing")
            continue
        text = texts.setdefault(script, p.read_text())
        if not written(text, call, gate):
            problems.append(f"{script}: no `{call} {gate}` receipt call")
    # The receipt library must be SOURCED by every writer script. A call to a
    # function that was never defined fails at runtime, not at parse time, so
    # `bash -n` cannot see it.
    for script in {s for s, _, _ in WRITERS}:
        text = texts.get(script)
        if text is None:
            continue
        if not _has_source(text):
            problems.append(f"{script}: does not source scripts/lib/gate-status.sh")

    # drift-watch installs its own EXIT trap for workdir cleanup AFTER the
    # receipt trap, and bash keeps only the newest — so the receipt is written
    # only if that final trap calls `gate_exit_trap`. First live run left the
    # receipt saying `running` forever. The LAST EXIT trap must call it.
    drift = texts.get("scripts/drift-watch.sh", "")
    if drift:
        traps = [
            ln.strip()
            for ln in drift.splitlines()
            if ln.strip().startswith("trap ") and "EXIT" in ln
            and not ln.strip().startswith("#")
        ]
        if not traps:
            problems.append("scripts/drift-watch.sh: no EXIT trap (receipt never written)")
        elif "gate_exit_trap" not in traps[-1]:
            problems.append(
                "scripts/drift-watch.sh: the last EXIT trap does not call "
                "gate_exit_trap (an earlier trap is overwritten by cleanup)"
            )

    main_rs = root / "crates/ocserve-cli/src/main.rs"
    if not main_rs.exists():
        problems.append("crates/ocserve-cli/src/main.rs: missing")
    elif not reader_wired(main_rs.read_text()):
        problems.append(
            "doctor does not fail on an unhealthy gate receipt "
            f"({READER_FN} → {READER_FAIL} → bail/exit)"
        )

    lib = root / "scripts/lib/gate-status.sh"
    if not lib.exists():
        problems.append("scripts/lib/gate-status.sh: missing")
    else:
        lib_text = lib.read_text()
        for fn in ("gate_begin", "gate_finish", "gate_sampler"):
            if not re.search(rf"^{fn}\(\)", lib_text, re.M):
                problems.append(f"scripts/lib/gate-status.sh: no `{fn}()`")
    return problems


def selftest() -> int:
    """Every plant must be caught, and the real tree must pass."""
    failures: list[str] = []

    real = check(ROOT)
    if real:
        failures.extend(f"real tree: {p}" for p in real)

    nightly = (ROOT / "scripts/nightly.sh").read_text()

    # Plant 1: a writer that no longer writes. Presence check alone.
    sabotaged = "\n".join(
        ln for ln in nightly.splitlines() if not ln.strip().startswith("gate_finish")
    )
    if written(sabotaged, "gate_finish", "nightly"):
        failures.append("plant 1 undetected: removed `gate_finish nightly` reads as wired")

    # Plant 2: the WRONG gate name. Proves the name check is real, not just a
    # presence check (a copy-paste would write the wrong receipt and doctor
    # would then call this gate fresh twice over).
    misnamed = nightly.replace("gate_begin nightly", "gate_begin soak")
    if written(misnamed, "gate_begin", "nightly"):
        failures.append("plant 2 undetected: `gate_begin soak` counted as the nightly receipt")

    # Plant 3: the reader computes the verdict and discards it.
    main_rs = (ROOT / "crates/ocserve-cli/src/main.rs").read_text()
    if reader_wired(main_rs.replace("if !gates_ok {", "if false {")):
        failures.append("plant 3 undetected: doctor ignoring an unhealthy gate reads as wired")

    # Plant 4: the writer never sources the receipt library. Bash resolves
    # functions at call time, so `bash -n` passes and the run dies mid-way.
    # Plant 5 (checked below, on the real drift script) is the trap overwrite.
    unsourced = "\n".join(
        ln for ln in nightly.splitlines() if "gate-status.sh" not in ln
    )
    src = [
        ln
        for ln in unsourced.splitlines()
        if "gate-status.sh" in ln and not ln.strip().startswith("#")
    ]
    if src and "." in src[0] and not _has_source(unsourced):
        failures.append("plant 4 undetected: unsourced writer reads as wired")

    # Plant 5: drift-watch's cleanup trap stops calling the receipt trap. This
    # is the bug the first live run exposed; it must never pass silently.
    drift = (ROOT / "scripts/drift-watch.sh").read_text()
    overwritten = drift.replace(
        "trap 'gate_exit_trap; _rc=$?; cleanup; rm -rf \"$WORK\"; exit $_rc' EXIT",
        "trap 'cleanup; rm -rf \"$WORK\"' EXIT",
    )
    traps = [
        ln.strip()
        for ln in overwritten.splitlines()
        if ln.strip().startswith("trap ") and "EXIT" in ln
    ]
    if traps and "gate_exit_trap" in traps[-1]:
        failures.append("plant 5 undetected: a cleanup trap overwriting the receipt trap reads as wired")

    if failures:
        print("FAIL: gate-receipt wiring check is vacuous or the tree regressed")
        for f in failures:
            print(f"  - {f}")
        return 1
    print(
        "ok (nightly/drift/soak write receipts naming their own gate; "
        "doctor fails on an unhealthy one; 5 plants caught)"
    )
    return 0


def main() -> int:
    args = sys.argv[1:]
    if args and args[0] == "--selftest":
        return selftest()
    problems = check(ROOT)
    if problems:
        for p in problems:
            print(f"FAIL: {p} (rule 61)")
        return 1
    print("ok (every scheduled gate writes a receipt; doctor fails on a bad one)")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
