#!/usr/bin/env python3
"""Load-fixture builder: subset snapshot of the legacy opencode.db (read-only).

Both load-test arms derive from ONE snapshot so their data volumes are
equivalent BY CONSTRUCTION:
  - freeze arm: snapshot installed as its native ~/.local/share/opencode/opencode.db
  - refine arm: `refine import --source <snapshot> --limit <big>`

Rules:
  - source is opened mode=ro (never written — the live install is sacred);
    one transaction = consistent WAL read snapshot
  - selection: --sessions most-recent non-archived sessions + the DEEPEST
    non-archived session (the 32k-message stress row, force-included)
  - project scoping (freeze lists per-project): lever 1 = cwd set to the
    dominant selected project's EXISTING worktree (no data rewrite);
    lever 2 = rewrite selected sessions to one synthetic project row for
    the repo cwd (fixture copy only) when that worktree is gone
  - snapshot lands on DISK (never tmpfs — the mutants-tmpfs lesson)

meta.json: counts, deep_sid, sids csv, cwd, lever — consumed by load-test.sh
and passed to k6 as env.
"""
from __future__ import annotations

import argparse
import hashlib
import json
import os
import sqlite3
import sys
import time

# tables copied whole (small / global) vs subset by selection
SUBSET_TABLES = {"session", "message", "part", "event", "event_sequence"}


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--source", required=True)
    ap.add_argument("--dest", required=True)
    ap.add_argument("--sessions", type=int, default=200)
    ap.add_argument("--meta", required=True)
    ap.add_argument("--cwd", default=os.getcwd(), help="repo root (arms share this cwd)")
    args = ap.parse_args()
    t0 = time.time()

    if not os.path.exists(args.source):
        print(f"fixture: source missing: {args.source}", file=sys.stderr)
        return 2

    src = sqlite3.connect(f"file:{args.source}?mode=ro", uri=True, timeout=30)
    src.execute("PRAGMA busy_timeout=30000")

    # ---- selection (one transaction: consistent WAL snapshot) ----
    src.execute("BEGIN")
    deepest = src.execute(
        """SELECT m.session_id FROM message m
           JOIN session s ON s.id = m.session_id
           WHERE s.time_archived IS NULL
           GROUP BY m.session_id ORDER BY COUNT(*) DESC LIMIT 1"""
    ).fetchone()
    recent = src.execute(
        """SELECT id FROM session WHERE time_archived IS NULL
           ORDER BY time_created DESC LIMIT ?""",
        (args.sessions,),
    ).fetchall()
    sel_ids = {r[0] for r in recent}
    if deepest:
        sel_ids.add(deepest[0])
    if not sel_ids:
        print("fixture: no sessions selected", file=sys.stderr)
        return 2

    # dominant project among selected → lever 1 if its worktree exists
    qmarks = ",".join("?" * len(sel_ids))
    dom = src.execute(
        f"""SELECT project_id, COUNT(*) c FROM session
            WHERE id IN ({qmarks}) GROUP BY project_id
            ORDER BY c DESC LIMIT 1""",
        tuple(sel_ids),
    ).fetchone()
    dom_pid, dom_count = (dom if dom else (None, 0))
    dom_wt = None
    if dom_pid:
        row = src.execute(
            "SELECT worktree FROM project WHERE id = ?", (dom_pid,)
        ).fetchone()
        dom_wt = row[0] if row else None
    lever = 1
    cwd = args.cwd
    if not (dom_wt and os.path.isdir(dom_wt)):
        lever = 2
        cwd = args.cwd  # arms run in the repo root

    # ---- build snapshot at dest.tmp then rename ----
    tmp = args.dest + ".tmp"
    if os.path.exists(tmp):
        os.remove(tmp)
    dst = sqlite3.connect(tmp)
    dst.execute("PRAGMA journal_mode=WAL")

    # schema (tables + explicit indexes)
    for name, sql in src.execute(
        "SELECT name, sql FROM sqlite_master WHERE sql IS NOT NULL"
    ):
        dst.execute(sql)
    # seed selection temp table in destination
    dst.execute("CREATE TABLE _sel (id TEXT PRIMARY KEY)")
    dst.executemany("INSERT OR IGNORE INTO _sel VALUES (?)", [(i,) for i in sel_ids])

    def copy_all(table: str) -> int:
        cur = dst.execute(f'INSERT INTO "{table}" SELECT * FROM src."{table}"')
        return cur.rowcount

    def copy_subset(table: str, col: str) -> int:
        cur = dst.execute(
            f'INSERT INTO "{table}" SELECT t.* FROM src."{table}" t '
            f"WHERE t.{col} IN (SELECT id FROM _sel)"
        )
        return cur.rowcount

    dst.execute("ATTACH ? AS src", (args.source,))
    counts: dict[str, int] = {}
    all_tables = [
        r[0]
        for r in src.execute(
            "SELECT name FROM sqlite_master WHERE type='table' AND name NOT LIKE 'sqlite_%'"
        )
    ]
    for t in all_tables:
        if t in SUBSET_TABLES:
            col = "session_id" if t in ("message", "part", "todo") else (
                "id" if t == "session" else "aggregate_id"
            )
            try:
                counts[t] = copy_subset(t, col)
            except sqlite3.Error as e:
                # event aggregates may key off message ids; best-effort second try
                if t == "event":
                    counts[t] = dst.execute(
                        """INSERT INTO event SELECT e.* FROM src.event e
                           WHERE e.aggregate_id IN (SELECT id FROM _sel)
                              OR e.aggregate_id IN (
                                 SELECT id FROM src.message
                                 WHERE session_id IN (SELECT id FROM _sel))"""
                    ).rowcount
                else:
                    raise RuntimeError(f"subset {t}: {e}") from e
        else:
            try:
                counts[t] = copy_all(t)
            except sqlite3.Error as e:
                raise RuntimeError(f"copy {t}: {e}") from e

    # lever 2: rewrite selected sessions to one synthetic project for cwd
    if lever == 2:
        pid = hashlib.sha1(cwd.encode()).hexdigest()[:32]
        src_cols = [r[1] for r in src.execute("PRAGMA table_info(project)")]
        src_row = src.execute(
            "SELECT * FROM project WHERE id = ?", (dom_pid,)
        ).fetchone() or src.execute("SELECT * FROM project LIMIT 1").fetchone()
        if src_row:
            row = list(src_row)
            for i, c in enumerate(src_cols):
                if c == "id":
                    row[i] = pid
                elif c == "worktree":
                    row[i] = cwd
                elif c == "name":
                    row[i] = "load-fixture"
                elif c in ("time_created", "time_updated", "time_initialized"):
                    row[i] = row[i]
            dst.execute(
                f"INSERT OR REPLACE INTO project VALUES ({','.join('?' * len(row))})",
                tuple(row),
            )
        dst.execute(
            "UPDATE session SET project_id = ?, directory = ? "
            "WHERE id IN (SELECT id FROM _sel)",
            (pid, cwd),
        )

    # indexes from source (explicit SQL only; UNIQUE autoindexes derive from DDL)
    for name, sql, tbl in src.execute(
        "SELECT name, sql, tbl_name FROM sqlite_master WHERE type='index' AND sql IS NOT NULL"
    ):
        try:
            dst.execute(sql.replace(f'"{tbl}"', f'"{tbl}"'))  # same names ok in fresh db
        except sqlite3.Error:
            pass  # index creation is best-effort; integrity_check is the gate

    dst.commit()
    dst.execute("DROP TABLE _sel")
    dst.commit()
    dst.execute("VACUUM")
    dst.commit()
    ok = dst.execute("PRAGMA integrity_check").fetchone()[0]
    dst.close()
    if ok != "ok":
        print(f"fixture: integrity_check failed: {ok}", file=sys.stderr)
        return 2
    os.replace(tmp, args.dest)

    # ---- meta for the harness / k6 ----
    deep = None
    if deepest:
        deep = deepest[0]
    # deep must be in the final set (non-archived guaranteed by the join)
    sids = list(sel_ids)
    # overweight the deep session ~5x in the k6 pool (it is the stress row;
    # spread mode otherwise gives it 1/N traffic)
    pool = sids + [deep] * 5 if deep else sids
    meta = {
        "sessions": len(sids),
        "counts": counts,
        "deep_sid": deep,
        "sids_csv": ",".join(sids),
        "pool_csv": ",".join(pool),
        "cwd": cwd,
        "lever": lever,
        "dominant_worktree": dom_wt,
        "source": args.source,
        "build_s": round(time.time() - t0, 1),
    }
    with open(args.meta, "w") as f:
        json.dump(meta, f, indent=1)
    print(
        f"fixture: {meta['sessions']} sessions, msgs={counts.get('message')}, "
        f"parts={counts.get('part')}, events={counts.get('event')} "
        f"(lever {lever}, {meta['build_s']}s)"
    )
    return 0


if __name__ == "__main__":
    sys.exit(main())
