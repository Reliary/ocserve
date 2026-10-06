#!/usr/bin/env python3
"""Server-side sampler for the load suite — runs DURING k6, 1 s ticks.

Per tick: loadavg1 (self-contamination record), per-arm process RSS + CPU
ticks (both arms are bare processes — fixture has plugin:[] so no children),
and refine-side gauges (upstream has no /metrics — parity's rule: cross-arm
metrics come from what each side can honestly expose).

Stops when the stopfile disappears. Final line = VmHWM peaks.
"""
from __future__ import annotations

import argparse
import json
import os
import sys
import time
import urllib.request


def proc_stat(pid: int) -> tuple[float, int] | tuple[None, None]:
    """(cpu_seconds, rss_bytes) or (None, None) if gone."""
    try:
        with open(f"/proc/{pid}/stat") as f:
            parts = f.read().rsplit(")", 1)[1].split()
        utime, stime = int(parts[11]), int(parts[12])
        with open(f"/proc/{pid}/status") as f:
            rss = next(
                int(l.split()[1]) * 1024
                for l in f
                if l.startswith("VmRSS:")
            )
        return (utime + stime) / os.sysconf("SC_CLK_TCK"), rss
    except (OSError, StopIteration, ValueError):
        return None, None


def vmhwm(pid: int) -> int | None:
    try:
        with open(f"/proc/{pid}/status") as f:
            return next(
                int(l.split()[1]) * 1024 for l in f if l.startswith("VmHWM:")
            )
    except (OSError, StopIteration, ValueError):
        return None


def metrics_snapshot(url: str) -> dict:
    out = {}
    try:
        text = urllib.request.urlopen(url + "/metrics", timeout=2).read().decode()
    except Exception:
        return out
    for line in text.splitlines():
        if line.startswith("#"):
            continue
        name, _, val = line.partition(" ")
        if name in (
            "refine_rss_bytes",
            "refine_writer_queue_depth",
            "refine_sse_clients",
            "refine_prompt_locks",
            "refine_db_opens_total",
        ):
            try:
                out[name] = float(val)
            except ValueError:
                pass
    return out


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--out", required=True)
    ap.add_argument("--stop", required=True)
    ap.add_argument("--pids", required=True, help="label=pid,label=pid")
    ap.add_argument("--refine-url", default="")
    args = ap.parse_args()
    pids: dict[str, int] = {}
    for kv in args.pids.split(","):
        if "=" in kv:
            label, pid = kv.split("=", 1)
            pids[label] = int(pid)
    start_cpu: dict[str, float] = {}
    samples = []
    with open(args.out, "w") as f:
        while not os.path.exists(args.stop):
            ts = time.time()
            try:
                load1 = float(open("/proc/loadavg").read().split()[0])
            except Exception:
                load1 = None
            row: dict = {"t": round(ts, 1), "load1": load1}
            for label, pid in pids.items():
                cpu, rss = proc_stat(pid)
                if label not in start_cpu and cpu is not None:
                    start_cpu[label] = cpu
                row[label] = {"cpu": cpu, "rss": rss}
            if args.refine_url:
                row["refine_metrics"] = metrics_snapshot(args.refine_url)
            samples.append(row)
            f.write(json.dumps(row) + "\n")
            f.flush()
            time.sleep(1)
        # final peaks
        peaks = {}
        for label, pid in pids.items():
            peaks[label] = {
                "vmhwm": vmhwm(pid),
                "cpu_seconds": start_cpu.get(label),
            }
        f.write(json.dumps({"final": peaks, "samples": len(samples)}) + "\n")
    return 0


if __name__ == "__main__":
    sys.exit(main())
