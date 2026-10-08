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
    # k6 v1 wrapped stats in {"values": {...}}; k6 v2 (the grafana/k6 image
    # we run) exports them FLAT — verified live against the image before the
    # first baseline (a v1-only parser would have produced an all-— table).
    if isinstance(m.get("values"), dict):
        return m["values"]
    return m if isinstance(m, dict) else {}


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
    lines.append("# Load report — ocserve vs upstream (k6 L1, fixture arms)")
    lines.append("")
    meta = {}
    if os.path.exists(f"{d}/meta.json"):
        meta = json.load(open(f"{d}/meta.json"))
    counts = meta.get("counts") or {}
    lines.append(
        f"- fixture: {meta.get('sessions', '?')} sessions "
        f"(msgs={counts.get('message', '?')}, parts={counts.get('part', '?')}), "
        f"deep={str(meta.get('deep_sid'))[:24]}…, lever={meta.get('lever')}"
    )
    cores = meta.get("cores") or {}
    if cores:
        lines.append(
            f"- cpu pinning (operator directive): class={cores.get('class')} · "
            f"ocserve=[{cores.get('ocserve')}] freeze=[{cores.get('freeze')}] "
            f"k6=[{cores.get('k6')}] — each arm owns whole physical cores "
            f"(both SMT threads), k6 on separate cores"
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
        for arm in ("ocserve", "freeze"):
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
        arm = next((p for p in parts if p in ("ocserve", "freeze")), "?")
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
        # k6 v2 flat export: failed RATE lives in .value (the .passes field
        # counts FAILED requests — verified against raw run1 exports where
        # ocserve value=0.1077 matched 516/4790 exactly)
        if "rate" not in failed and "value" in failed:
            failed = dict(failed, rate=failed["value"])
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
    mems = [r["mem_kb"] for r in samples if r.get("mem_kb") is not None]
    if mems:
        lines.append(
            f"- MemAvailable during run: min {round(min(mems)/1048576,1)} GB "
            f"(co-tenant guard refuses runs below 1.5 GB at start)"
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
    for arm in ("ocserve", "freeze"):
        if arm in peak_rss:
            cpu = None
            if arm in cpu_delta and arm + "_start" in cpu_delta:
                cpu = round(cpu_delta[arm] - cpu_delta[arm + "_start"], 1)
            lines.append(
                f"- {arm}: peak RSS {round(peak_rss[arm]/1048576,1)} MB, "
                f"cpu {cpu if cpu is not None else '—'} s over sampled window"
            )
    # ocserve-side gauges from last sample with metrics
    last_m = {}
    for row in reversed(samples):
        if row.get("ocserve_metrics"):
            last_m = row["ocserve_metrics"]
            break
    if last_m:
        lines.append(
            f"- ocserve end-state gauges: "
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
