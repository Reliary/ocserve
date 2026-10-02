#!/usr/bin/env python3
"""Cgroup-v2 resource sampler for a docker container.

Primary source: direct cgroup file reads (memory.current, memory.stat anon,
cpu.stat usage_usec) resolved from `docker inspect` → State.Pid →
/proc/<pid>/cgroup. Fallback: `docker stats --no-stream`. Which source was
used is recorded — never silently mixed.
"""
from __future__ import annotations

import json
import os
import subprocess
import threading
import time


def _sh(args, timeout=15):
    return subprocess.run(args, capture_output=True, text=True, timeout=timeout)


def cgroup_paths(container: str) -> dict | None:
    """Resolve readable cgroup v2 file paths for a container (or None)."""
    out = _sh(["docker", "inspect", "-f", "{{.State.Pid}}", container])
    if out.returncode != 0:
        return None
    pid = out.stdout.strip()
    if not pid or pid == "0":
        return None
    try:
        cg = open(f"/proc/{pid}/cgroup").read().strip()
    except OSError:
        return None
    # cgroup v2 single hierarchy: "0::/system.slice/docker-<id>.scope"
    rel = None
    for line in cg.splitlines():
        if line.startswith("0::"):
            rel = line.split("::", 1)[1]
            break
    if rel is None:
        return None
    base = "/sys/fs/cgroup" + rel
    files = {
        "current": f"{base}/memory.current",
        "peak": f"{base}/memory.peak",
        "stat": f"{base}/memory.stat",
        "cpu": f"{base}/cpu.stat",
    }
    if all(os.path.exists(f) for f in (files["current"], files["stat"], files["cpu"])):
        return files
    return None


def _read_kv(path: str) -> dict:
    out = {}
    try:
        for line in open(path):
            parts = line.split()
            if len(parts) >= 2:
                out[parts[0]] = int(parts[1])
    except (OSError, ValueError):
        pass
    return out


class Sampler:
    """1 Hz sampler thread → CSV; stop() returns a summary dict."""

    def __init__(self, container: str, csv_path: str, interval: float = 1.0):
        self.container = container
        self.csv_path = csv_path
        self.interval = interval
        self.rows: list[tuple] = []
        self.source = "cgroup"
        self._paths = cgroup_paths(container)
        if self._paths is None:
            self.source = "docker-stats"
        self._stop = threading.Event()
        self._thread = threading.Thread(target=self._run, daemon=True)
        self._err: str | None = None

    def start(self):
        self._thread.start()

    def _sample(self):
        if self.source == "cgroup":
            p = self._paths
            current = int(open(p["current"]).read())
            stat = _read_kv(p["stat"])
            cpu = _read_kv(p["cpu"])
            return (time.time(), current, stat.get("anon", 0),
                    cpu.get("usage_usec", 0))
        # fallback
        out = _sh(["docker", "stats", "--no-stream", "--format",
                   "{{json .}}", self.container])
        try:
            d = json.loads(out.stdout)
            mem = int(d["MemUsage"].split("/")[0].strip().rstrip("iB").rstrip("B")
                      .replace("Ki", "").replace("Mi", "").replace("Gi", "")) or 0
            # docker stats strings are messy — parse conservatively
            return (time.time(), mem, 0, 0)
        except Exception:
            return (time.time(), -1, -1, -1)

    def _run(self):
        try:
            with open(self.csv_path, "w") as f:
                f.write("ts,current,anon,cpu_usec\n")
                while not self._stop.is_set():
                    row = self._sample()
                    self.rows.append(row)
                    f.write("%d,%d,%d,%d\n" % row)
                    f.flush()
                    self._stop.wait(self.interval)
        except Exception as e:  # loud, never silent
            self._err = f"{type(e).__name__}: {e}"

    def stop(self) -> dict:
        self._stop.set()
        self._thread.join(timeout=10)
        summary = {
            "source": self.source,
            "samples": len(self.rows),
            "error": self._err,
            "peak_current": None,
            "peak_anon": None,
            "cpu_seconds": None,
        }
        if self.rows:
            summary["peak_current"] = max(r[1] for r in self.rows if r[1] >= 0) or None
            summary["peak_anon"] = max(r[2] for r in self.rows if r[2] >= 0) or None
            cpus = [r[3] for r in self.rows if r[3] >= 0]
            if len(cpus) >= 2 and self.source == "cgroup":
                summary["cpu_seconds"] = round((cpus[-1] - cpus[0]) / 1e6, 3)
        # cgroup memory.peak (kernel-tracked, includes spikes between samples)
        if self._paths and os.path.exists(self._paths.get("peak", "")):
            try:
                summary["cgroup_memory_peak"] = int(open(self._paths["peak"]).read())
            except (OSError, ValueError):
                pass
        return summary

    def slope_mb_per_hour(self) -> float | None:
        """Least-squares slope of anon bytes/h — the leak detector (S4)."""
        pts = [(r[0], float(r[2])) for r in self.rows if r[2] > 0]
        if len(pts) < 10:
            return None
        n = len(pts)
        t0 = pts[0][0]
        xs = [p[0] - t0 for p in pts]
        ys = [p[1] for p in pts]
        mx = sum(xs) / n
        my = sum(ys) / n
        den = sum((x - mx) ** 2 for x in xs)
        if den == 0:
            return None
        b = sum((x - mx) * (y - my) for x, y in zip(xs, ys)) / den
        return round(b * 3600 / (1024 * 1024), 4)  # MB/h
