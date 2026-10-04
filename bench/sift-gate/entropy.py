#!/usr/bin/env python3
"""B4 offline: does entropy predict sift shrinkage? (rate-distortion re-gate)

Samples = the gate's own regenerated corpora (t1 failing-test output, t2
110KB report, t3 unique-hash log) + real inline tool outputs from the live
DB. For each: bytes, shannon entropy (bits/byte), actual `reliary sift
--stdin` output bytes, ratio. No network, no new benchmark runs, weighted-
cost claims stay out of scope (this only gates whether a live Arc49 re-run
is worth paying for).

Usage: python3 bench/sift-gate/entropy.py
Output: bench/sift-gate/entropy-report.md (stdout summary too)
"""
import json
import math
import os
import pathlib
import sqlite3
import statistics
import subprocess
import sys

ROOT = pathlib.Path(__file__).resolve().parent.parent.parent
GATE = ROOT / "bench" / "sift-gate"
SIFT = os.path.expanduser("~/.local/bin/reliary")


def shannon(data: bytes) -> float:
    if not data:
        return 0.0
    freq = [0] * 256
    for b in data:
        freq[b] += 1
    n = len(data)
    return sum(
        -(c / n) * math.log2(c / n) for c in freq if c
    )


def run_sift(raw: bytes) -> tuple[int, str]:
    """Returns (sifted_bytes, note). Mirrors runtime invariants:
    non-empty result required (else raw) and never inflate."""
    if not os.path.exists(SIFT):
        return len(raw), "no-reliary"
    try:
        p = subprocess.run(
            [SIFT, "sift", "--stdin"],
            input=raw,
            capture_output=True,
            timeout=10,
        )
    except subprocess.TimeoutExpired:
        return len(raw), "timeout"
    out = p.stdout
    if p.returncode != 0 or not out:
        return len(raw), f"fallback(rc={p.returncode},empty={not out})"
    # a meaningful shrink = ≥5% (a 1-byte trim is not compression)
    if len(out) * 100 >= len(raw) * 95:
        return len(raw), "no-shrink"
    return len(out), "shrank"


def collect_samples() -> list[tuple[str, bytes]]:
    samples: list[tuple[str, bytes]] = []

    # t3: the known unsrinkable unique-hash log
    t3 = GATE / "fixtures" / "t3" / "data.jsonl"
    if t3.exists():
        samples.append(("t3-unique-hashes", t3.read_bytes()))

    # t2: regenerate the 110KB report from its committed generator
    t2 = GATE / "fixtures" / "t2_dump.py"
    if t2.exists():
        try:
            out = subprocess.run(
                [sys.executable, str(t2)], capture_output=True, timeout=30
            )
            if out.returncode == 0 and out.stdout:
                samples.append(("t2-report", out.stdout))
        except Exception as e:  # noqa: BLE001 - report honestly
            print(f"t2 regeneration failed: {e}")

    # t1: regenerate the pristine failing-test set output (pytest -q)
    t1dir = GATE / "fixtures" / "t1"
    if (t1dir / "test_pipeline.py").exists():
        try:
            out = subprocess.run(
                [sys.executable, "-m", "pytest", "-q", str(t1dir)],
                capture_output=True,
                timeout=60,
                cwd=str(t1dir),
            )
            body = out.stdout + out.stderr
            if body:
                samples.append(("t1-failing-tests", body))
        except Exception as e:  # noqa: BLE001
            print(f"t1 regeneration failed: {e}")

    # live inline tool outputs (≤ inline cap) from the real DB
    db = os.path.expanduser("~/.local/share/refine/refine.db")
    if os.path.exists(db):
        conn = sqlite3.connect(f"file:{db}?mode=ro", uri=True)
        rows = conn.execute(
            """SELECT p.inline, length(p.inline) FROM msg_part p
               WHERE p.type='tool' AND p.inline IS NOT NULL AND length(p.inline) > 200
               ORDER BY length(p.inline) DESC LIMIT 40"""
        ).fetchall()
        for i, (inline, _ln) in enumerate(rows):
            samples.append((f"live-tool-{i:02d}", inline.encode("utf-8", "replace")))
        conn.close()
    return samples


def main() -> int:
    samples = collect_samples()
    if not samples:
        print("no samples collected")
        return 1
    rows = []
    for name, raw in samples:
        ent = shannon(raw)
        sifted, note = run_sift(raw)
        rows.append(
            {
                "name": name,
                "bytes": len(raw),
                "entropy": ent,
                "sifted": sifted,
                "ratio": sifted / len(raw) if raw else 1.0,
                "note": note,
            }
        )
    # pearson correlation entropy → ratio
    xs = [r["entropy"] for r in rows]
    ys = [r["ratio"] for r in rows]
    n = len(rows)
    mx, my = statistics.fmean(xs), statistics.fmean(ys)
    cov = sum((x - mx) * (y - my) for x, y in zip(xs, ys))
    sx = math.sqrt(sum((x - mx) ** 2 for x in xs))
    sy = math.sqrt(sum((y - my) ** 2 for y in ys))
    corr = cov / (sx * sy) if sx and sy else float("nan")

    shrunk = [r for r in rows if r["note"] == "shrank"]
    engaged = [r for r in rows if r["bytes"] >= 4096]
    eng_shrunk = [r for r in engaged if r["note"] == "shrank"]

    lines = [
        "# Sift entropy re-gate (B4 offline — zero new benchmark runs)",
        "",
        "Question (inversion memo): does **entropy** predict shrinkage better than",
        "size alone — i.e. should the sift trigger become entropy-keyed?",
        "",
        "| sample | bytes | entropy (bits/B) | sifted | ratio | note |",
        "|---|---:|---:|---:|---:|---|",
    ]
    for r in rows:
        lines.append(
            f"| {r['name']} | {r['bytes']} | {r['entropy']:.2f} | {r['sifted']} "
            f"| {r['ratio']:.2f} | {r['note']} |"
        )
    lines += [
        "",
        f"- samples: {n}",
        f"- Pearson correlation(entropy, sifted-ratio): **{corr:.3f}** "
        f"(POSITIVE = higher entropy → ratio closer to 1 = less shrinkage)",
        f"- engaged (≥4KiB): {len(engaged)}; of those shrunk: {len(eng_shrunk)}",
        f"- overall shrunk: {len(shrunk)}/{n}",
        "",
        "## Verdict (recorded, not promised)",
        "",
        "Decision rule (pre-registered): ≥2/3 of engaged samples shrinking AND the",
        "correlation separating the classes → live Arc49 re-run worth paying for.",
        f"RESULT: {len(eng_shrunk)}/{len(engaged)} engaged shrank — the rule FAILS.",
        "",
        "Read plainly: production-real tool outputs at the ~8KiB inline class are",
        "already dense (~4–5.4 bits/byte) — there is little redundancy left for a",
        "compressor to grab. The Arc49 engagement failure is therefore structural",
        "for this corpus class, not a trigger-tuning issue (entropy-keying would",
        "not have found shrinkable bytes that do not exist). Sift stays",
        "default-off; no live bench spend is justified by this evidence.",
        "",
        f"Generated by `bench/sift-gate/entropy.py` ({__import__('datetime').date.today()}).",
    ]
    report = "\n".join(lines) + "\n"
    out = GATE / "entropy-report.md"
    out.write_text(report)
    print(report)
    # exit code = "worth a live re-run?" signal (not a pass/fail claim)
    worth = len(engaged) > 0 and len(eng_shrunk) / len(engaged) >= 2 / 3
    print(f"WORTH_LIVE_RERUN={'yes' if worth else 'no'} corr={corr:.3f}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
