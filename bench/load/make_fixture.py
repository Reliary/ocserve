#!/usr/bin/env python3
"""Load-fixture builder: subset snapshot of the legacy opencode.db (read-only).

Both load-test arms derive from ONE snapshot so their data volumes are
equivalent BY CONSTRUCTION:
  - freeze arm: snapshot installed as its native ~/.local/share/opencode/opencode.db
  - ocserve arm: `ocserve import --source <snapshot> --limit <big>`

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

    def phase(msg: str) -> None:
        print(f"[{time.time()-t0:6.1f}s] {msg}", flush=True)

    if not os.path.exists(args.source):
        print(f"fixture: source missing: {args.source}", file=sys.stderr)
        return 2

    src = sqlite3.connect(f"file:{args.source}?mode=ro", uri=True, timeout=30)
    src.execute("PRAGMA busy_timeout=30000")

    # ---- selection (one transaction: consistent WAL snapshot) ----
    src.execute("BEGIN")
    phase("selection started")
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
    phase(f"selection done ({len(sel_ids)} ids)")

    # Scoping (lever 3 — proven empirically 2026-10-06): freeze lists by
    # listByProject(ctx.project.id) and project ids are GIT-DERIVED (the
    # stored ocserve-project id is literally commit 8b87603…, our M0) — so a
    # boot-time cwd never matches stored project ids (first run showed
    # freeze=0). The only STABLE id is the literal 'global' project
    # (worktree '/'), which already holds 186/201 selected sessions: rewrite
    # ALL selected sessions to it and boot both arms at cwd='/'.
    lever = 3
    cwd = "/"

    # ---- build snapshot at dest.tmp then rename ----
    tmp = args.dest + ".tmp"
    for stale in (tmp, tmp + "-wal", tmp + "-shm"):
        if os.path.exists(stale):
            os.remove(stale)
    dst = sqlite3.connect(tmp)
    dst.execute("PRAGMA journal_mode=WAL")
    # disposable build: integrity_check gates before rename, so skip fsync
    # (crash mid-build = discard tmp; synchronous=OFF = ~10x write speedup)
    dst.execute("PRAGMA synchronous=OFF")

    # schema (tables + explicit indexes)
    for name, sql in src.execute(
        "SELECT name, sql FROM sqlite_master WHERE sql IS NOT NULL"
    ):
        # sqlite_sequence etc are kernel-internal names (CREATE of them is
        # reserved — caught red on the first live fixture build); TEXT pks
        # make sequence bookkeeping irrelevant anyway
        if name.startswith("sqlite_"):
            continue
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

    def copy_events() -> int:
        # materialize selected message ids ONCE — the previous per-row
        # correlated subquery over 641k events was the slow path (and with
        # the stale-WAL bug above, exceeded 400s twice)
        dst.execute(
            "CREATE TEMP TABLE _msg_sel AS SELECT id FROM src.message "
            "WHERE session_id IN (SELECT id FROM _sel)"
        )
        n = dst.execute(
            """INSERT INTO event SELECT e.* FROM src.event e
               WHERE e.aggregate_id IN (SELECT id FROM _sel)
                  OR e.aggregate_id IN (SELECT id FROM _msg_sel)"""
        ).rowcount
        dst.execute("DROP TABLE _msg_sel")
        return n

    dst.execute("ATTACH ? AS src", (args.source,))
    phase("schema + attach done")
    counts: dict[str, int] = {}
    all_tables = [
        r[0]
        for r in src.execute(
            "SELECT name FROM sqlite_master WHERE type='table' AND name NOT LIKE 'sqlite_%'"
        )
    ]
    for t in all_tables:
        if t == "event":
            # Historical event PAYLOADS are ~2GB (643k rows × multi-KB) and
            # L1 never reads them: session/message/part routes don't touch the
            # event table, and live events regenerate identically on both
            # arms from the same seed activity (zero in L1). Schema +
            # event_sequence still copied; measured count recorded as 0.
            counts[t] = 0
            continue
        if t in SUBSET_TABLES:
            col = "session_id" if t in ("message", "part", "todo") else (
                "id" if t == "session" else "aggregate_id"
            )
            if t == "event":
                counts[t] = copy_events()
            else:
                try:
                    counts[t] = copy_subset(t, col)
                except sqlite3.Error as e:
                    raise RuntimeError(f"subset {t}: {e}") from e
        else:
            try:
                counts[t] = copy_all(t)
            except sqlite3.Error as e:
                raise RuntimeError(f"copy {t}: {e}") from e
        phase(f"copied {t}: {counts[t]} rows")

    # lever 3: pin every selected session to the stable 'global' project
    # (fixture copy only — the source is opened read-only)
    has_global = dst.execute(
        "SELECT 1 FROM project WHERE id='global' LIMIT 1"
    ).fetchone()
    if not has_global:
        dst.execute(
            "INSERT INTO project (id, worktree) VALUES ('global', '/')"
        )
    # directory is ALSO rewritten: freeze re-homes rows whose `directory`
    # points at a registered worktree (observed live: 2 sessions left
    # 'global' into project 289e1767… mid-run, breaking count equality) —
    # directory='/' anchors every row to the cwd we boot at.
    dst.execute(
        "UPDATE session SET project_id='global', directory='/' "
        "WHERE id IN (SELECT id FROM _sel)"
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
    # no VACUUM: INSERT-SELECT into a fresh db is already sequential —
    # VACUUM rewrote gigabytes through WAL and was a time-killer.
    # DETACH first: integrity_check validates ALL ATTACHED databases — with
    # the 33GB source attached it ran >250s (hung in three timed runs;
    # standalone on the same file: 8s). Main-only check after detach.
    dst.execute("DETACH src")
    phase("integrity_check running (main only)")
    ok = dst.execute("PRAGMA integrity_check(20)").fetchone()[0]
    phase(f"integrity_check={ok}")
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
        "file_path": "/tmp",
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
