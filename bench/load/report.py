#!/usr/bin/env python3
"""Load-suite report: merge k6 summary-exports + sampler JSON → report.md.

Claim classes (README): capacity (achieved vs offered) and error rates are
valid at n=1; latency deltas between arms are INDICATIVE until ROUNDS>=3 —
printed with every latency table so numbers never outgrow their evidence.
"""
from __future__ import annotations

import argparse
import glob
import json
import os
import sys


def k6_metric(summary: dict, name: str) -> dict:
    m = (summary.get("metrics") or {}).get(name) or {}
    return m.get("values") or {}


def num(d: dict, key: str, scale: float = 1.0):
    v = d.get(key)
    if v is None:
        return "—"
    return round(float(v) * scale, 3)


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--dir", required=True, help="run directory")
    args = ap.parse_args()
    d = args.dir
    lines: list[str] = []
    lines.append("# Load report — refine vs upstream (k6 L1, fixture arms)")
    lines.append("")
    meta = {}
    if os.path.exists(f"{d}/meta.json"):
        meta = json.load(open(f"{d}/meta.json"))
    lines.append(
        f"- fixture: {meta.get('sessions', '?')} sessions "
        f"(msgs={meta.get('msgs', '?')}, parts={meta.get('parts', '?')}), "
        f"deep={str(meta.get('deep_sid'))[:24]}…, lever={meta.get('lever')}"
    )
    lines.append(f"- run order (rounds interleaved): {meta.get('order', '—')}")
    lines.append(f"- ramp targets (concurrency ladder): {meta.get('targets', '—')}")
    lines.append(
        "- **claim classes**: achieved-capacity + error-rate valid at n=1; "
        "arm-to-arm latency deltas are INDICATIVE until ROUNDS>=3 (2.7× rule)"
    )
    lines.append("")

    # ---- per k6 export: results table ----
    lines.append("## Results (per arm × mode × scenario script)")
    lines.append("")
    lines.append(
        "| arm | mode | script | p50 ms | p95 ms | p99 ms | reqs | failed | "
        "req/s | peak RSS MB | cpu s |"
    )
    lines.append("|---|---|---|---:|---:|---:|---:|---:|---:|---:|---:|")
    samples = []
    if os.path.exists(f"{d}/samples.jsonl"):
        for line in open(f"{d}/samples.jsonl"):
            try:
                samples.append(json.loads(line))
            except json.JSONDecodeError:
                pass
    peak_rss = {}
    cpu_delta = {}
    for row in samples:
        for arm in ("refine", "freeze"):
            st = row.get(arm) or {}
            rss, cpu = st.get("rss"), st.get("cpu")
            if rss:
                peak_rss[arm] = max(peak_rss.get(arm, 0), rss)
            if cpu is not None and arm not in cpu_delta:
                cpu_delta[arm + "_start"] = cpu
            if cpu is not None:
                cpu_delta[arm] = max(cpu_delta.get(arm, 0), cpu)
    loads = [r["load1"] for r in samples if r.get("load1") is not None]

    exports = sorted(glob.glob(f"{d}/*.summary.json"))
    if not exports:
        lines.append("| — | — | no k6 exports found | | | | | | | | |")
    for path in exports:
        base = os.path.basename(path).replace(".summary.json", "")
        # file naming: <round>-<arm>-<mode>-<script>
        parts = base.split("-")
        arm = next((p for p in parts if p in ("refine", "freeze")), "?")
        mode = next((p for p in parts if p in ("spread", "hot", "arrival")), "?")
        script = "arrival" if "arrival" in base else "read-hot"
        try:
            s = json.load(open(path))
        except json.JSONDecodeError:
            lines.append(f"| {arm} | {mode} | {script} | corrupt export | | | | | | | |")
            continue
        dur = k6_metric(s, "http_req_duration")
        reqs = k6_metric(s, "http_reqs")
        failed = k6_metric(s, "http_req_failed")
        peak = peak_rss.get(arm)
        cpu = None
        if arm in cpu_delta and arm + "_start" in cpu_delta:
            cpu = round(cpu_delta[arm] - cpu_delta[arm + "_start"], 1)
        lines.append(
            f"| {arm} | {mode} | {script} | {num(dur,'p(50)')} | {num(dur,'p(95)')} "
            f"| {num(dur,'p(99)')} | {int(float(reqs.get('count', 0)))} "
            f"| {num(failed, 'rate', 100)}% | {num(reqs, 'rate')} "
            f"| {round(peak/1048576,1) if peak else '—'} | {cpu if cpu is not None else '—'} |"
        )
    lines.append("")
    if loads:
        lines.append(
            f"- loadavg1 during run: min {min(loads)} / max {max(loads)} "
            f"(quiet-host gate applies at start; recorded for latency context)"
        )
    lines.append("")

    # ---- per-endpoint trends (informational) ----
    lines.append("## Per-endpoint p95 (informational — pooled p95 is the gate)")
    lines.append("")
    endpoints = [
        "session_list", "message_page", "message_cursor", "config",
        "agent", "command", "file_list", "search", "session_status",
    ]
    lines.append("| export | " + " | ".join(endpoints) + " |")
    lines.append("|---|" + "---:|" * len(endpoints))
    for path in exports:
        base = os.path.basename(path).replace(".summary.json", "")
        try:
            s = json.load(open(path))
        except json.JSONDecodeError:
            continue
        cells = []
        for e in endpoints:
            v = k6_metric(s, "lat_" + e)
            cells.append(str(num(v, "p(95)")))
        lines.append(f"| {base} | " + " | ".join(cells) + " |")
    lines.append("")

    lines.append("## Sampler peaks")
    lines.append("")
    for arm in ("refine", "freeze"):
        if arm in peak_rss:
            cpu = None
            if arm in cpu_delta and arm + "_start" in cpu_delta:
                cpu = round(cpu_delta[arm] - cpu_delta[arm + "_start"], 1)
            lines.append(
                f"- {arm}: peak RSS {round(peak_rss[arm]/1048576,1)} MB, "
                f"cpu {cpu if cpu is not None else '—'} s over sampled window"
            )
    # refine-side gauges from last sample with metrics
    last_m = {}
    for row in reversed(samples):
        if row.get("refine_metrics"):
            last_m = row["refine_metrics"]
            break
    if last_m:
        lines.append(
            f"- refine end-state gauges: "
            + ", ".join(f"{k}={v:g}" for k, v in sorted(last_m.items()))
        )
    lines.append("")

    report = "\n".join(lines)
    with open(f"{d}/report.md", "w") as f:
        f.write(report + "\n")
    print(report)
    return 0


if __name__ == "__main__":
    sys.exit(main())
