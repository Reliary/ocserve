#!/usr/bin/env python3
"""Aggregate round results → report.md (medians + spread, no invented gates).

Pairing: rounds named <tag>u / <tag>r (upstream/refine). Warmup pairs are
listed but excluded from medians when their tag starts with 'warm'.
A missing result file or a `null` metric stays `null` (decidability).
"""
from __future__ import annotations

import glob
import json
import os
import statistics
import sys

BENCH = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))


def load():
    runs = {}
    for path in glob.glob(os.path.join(BENCH, ".runs", "*", "*", "result.json")):
        with open(path) as f:
            d = json.load(f)
        runs[(d["round"], d["arm"])] = d
    return runs


def med(xs):
    xs = [x for x in xs if x is not None]
    return statistics.median(xs) if xs else None


def spread(xs):
    xs = [x for x in xs if x is not None]
    if len(xs) < 2:
        return None
    m = statistics.median(xs)
    # sub-5ms medians live in timer-jitter territory: a relative spread
    # there is noise amplification, not a stability signal
    if m == 0 or (abs(m) < 0.005 and all(abs(x) < 0.05 for x in xs)):
        return None
    return round((max(xs) - min(xs)) / m, 3)


def fmt(v, unit=""):
    if v is None:
        return "—"
    if isinstance(v, float):
        return f"{v:.3f}{unit}" if abs(v) < 100 else f"{v:.1f}{unit}"
    return f"{v}{unit}"


def main():
    runs = load()
    if not runs:
        print("no results under .runs/", file=sys.stderr)
        sys.exit(1)
    rounds = sorted({r for r, _ in runs})
    measured = [r for r in rounds if not r.startswith("warm")]
    warmups = [r for r in rounds if r.startswith("warm")]
    arms = ["upstream", "refine"]

    def pair_tags():
        """m1u/m1r → (m1, m1u, m1r): arm-runs have DISTINCT round ids."""
        tags = {}
        for r in measured:
            tag, arm_suf = r[:-1], r[-1]
            arm = "upstream" if arm_suf == "u" else "refine"
            tags.setdefault(tag, {})[arm] = r
        return tags

    def series(round_list, arm, getter):
        return [getter(runs[(r, arm)]) for r in round_list if (r, arm) in runs]

    # metric extraction
    def m_boot(d):     return d.get("scenarios", {}).get("S0", {}).get("boot_s")
    def m_idle(d):     return (d.get("resources") or {}).get("peak_current")
    def m_anon(d):     return (d.get("resources") or {}).get("peak_anon")
    def m_cpu(d):      return (d.get("resources") or {}).get("cpu_seconds")
    def m_s1p50(d):    return d.get("scenarios", {}).get("S1", {}).get("latency_s", {}).get("p50")
    def m_s1p95(d):    return d.get("scenarios", {}).get("S1", {}).get("latency_s", {}).get("p95")
    def _s4_window_slope(d):
        """Slope over the FINAL idle window only — recomputed from the CSV.
        The stored value fit the whole sampled window (load phase included),
        which is not what 'post-load idle slope' means."""
        idle = d.get("scenarios", {}).get("S4", {}).get("idle_s")
        path = os.path.join(BENCH, ".runs", d["round"], d["arm"], "sampler.csv")
        if not idle or not os.path.exists(path):
            return d.get("scenarios", {}).get("S4", {}).get("anom_slope_mb_per_h")
        rows = []
        try:
            with open(path) as fh:
                next(fh, None)
                for line in fh:
                    parts = line.strip().split(",")
                    if len(parts) == 4:
                        rows.append((float(parts[0]), float(parts[2])))
        except OSError:
            return None
        if len(rows) < 10:
            return None
        t_end = rows[-1][0]
        win = [(t, a) for t, a in rows if t >= t_end - float(idle) and a > 0]
        if len(win) < 5:
            return None
        t0 = win[0][0]
        xs = [t - t0 for t, _ in win]
        ys = [a for _, a in win]
        n = len(xs)
        mx, my = sum(xs) / n, sum(ys) / n
        den = sum((x - mx) ** 2 for x in xs)
        if den == 0:
            return None
        b = sum((x - mx) * (y - my) for x, y in zip(xs, ys)) / den
        return round(b * 3600 / (1024 * 1024), 4)

    def m_s4(d):       return _s4_window_slope(d)
    def m_db(d):       return d.get("db_bytes")
    def m_s3lag(d):    return d.get("scenarios", {}).get("S3", {}).get("lag_p95")
    def m_s3eps(d):    return d.get("scenarios", {}).get("S3", {}).get("events_per_s")

    def s2_p95(route):
        def g(d):
            return d.get("scenarios", {}).get("S2", {}).get(route, {}).get("total_p95")
        return g

    lines = []
    lines.append("# Parity report — upstream opencode vs refine\n")
    meta = runs[(rounds[-1], "refine" if (rounds[-1], "refine") in runs else rounds[-1] and "upstream")]
    lines.append(f"- refine sha: `{meta.get('refine_git_sha')}`  "
                 f"rounds: {len(measured)} measured (+{len(warmups)} warmup)  "
                 f"generated: {meta.get('finished_utc')}")
    lines.append(f"- quiet-host gate: load1 ≤ {meta.get('max_load1')} "
                 f"(start load1 {meta.get('host_load1_start')}, "
                 f"end {meta.get('host_load1_end')})")
    lines.append("")
    lines.append("Medians over measured rounds; **spread** = (max−min)/median "
                 "of that arm's rounds. `—` = metric did not run (never 0).\n")

    metrics = [
        ("Boot → healthy (s)", m_boot, ""),
        ("Idle current peak (B)", m_idle, ""),
        ("Anon peak (B)", m_anon, ""),
        ("CPU-seconds (measured window)", m_cpu, ""),
        ("S1 prompt p50 (s)", m_s1p50, ""),
        ("S1 prompt p95 (s)", m_s1p95, ""),
        ("S3 write→event lag p95 (s)", m_s3lag, ""),
        ("S3 events/s", m_s3eps, ""),
        ("S4 post-load anon slope (MB/h, idle window)", m_s4, ""),
        ("DB+WAL bytes (end)", m_db, ""),
        ("S2 session_list p95 (s)", s2_p95("session_list"), ""),
        ("S2 message_page p95 (s)", s2_p95("message_page"), ""),
        ("S2 message_cursor p95 (s)", s2_p95("message_cursor"), ""),
        ("S2 file_list p95 (s)", s2_p95("file_list"), ""),
        ("S2 config p95 (s)", s2_p95("config"), ""),
        ("S2 agent p95 (s)", s2_p95("agent"), ""),
        ("S2 command p95 (s)", s2_p95("command"), ""),
    ]

    lines.append("| metric | upstream | spread | refine | spread | Δ(ref/up−1) |")
    lines.append("|---|---:|---:|---:|---:|---:|")
    for label, getter, _u in metrics:
        up = series(measured, "upstream", getter)
        rf = series(measured, "refine", getter)
        up_m, rf_m = med(up), med(rf)
        delta = None
        if up_m not in (None, 0) and rf_m is not None:
            delta = f"{(rf_m / up_m - 1) * 100:+.1f}%"
        lines.append(f"| {label} | {fmt(up_m)} | {fmt(spread(up))} "
                     f"| {fmt(rf_m)} | {fmt(spread(rf))} | {delta or '—'} |")
    lines.append("")

    # seed parity per pair
    lines.append("## Seed parity (byte-identical histories across arms?)\n")
    lines.append("| pair | upstream digests | refine digests | identical |")
    lines.append("|---|---:|---:|---|")
    for tag, arms_map in sorted(pair_tags().items()):
        u = runs.get((arms_map.get("upstream", ""), "upstream"))
        f = runs.get((arms_map.get("refine", ""), "refine"))
        if not u or not f:
            continue
        du, df = u.get("seed_digests"), f.get("seed_digests")
        same = du is not None and du == df
        lines.append(f"| {tag} | {len(du) if du else '—'} "
                     f"| {len(df) if df else '—'} | {'✅' if same else ('❌' if du and df else '—')} |")
    lines.append("")

    # errors + resource samples
    lines.append("## Errors & resource samples\n")
    for (r, arm), d in sorted(runs.items()):
        errs = d.get("errors", [])
        res = d.get("resources") or {}
        lines.append(f"- **{r}/{arm}**: samples={res.get('samples')} "
                     f"src={res.get('source')} peak_cur={fmt(res.get('peak_current'))} "
                     f"cgroup_peak={fmt(res.get('cgroup_memory_peak'))} "
                     f"err={'; '.join(errs) if errs else 'none'}")
    lines.append("")
    out = os.path.join(BENCH, ".runs", "report.md")
    with open(out, "w") as fh:
        fh.write("\n".join(lines))
    print(f"wrote {out}")


if __name__ == "__main__":
    main()
