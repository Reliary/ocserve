#!/usr/bin/env python3
"""E1 search-shape shootout (PERF-10X Phase I).

Run against a WRITABLE copy of the fixture db (scratch.db). Measures:
  BASE  — current search_parts shape (rowid IN fts-set + join + sort with text)
  P-A   — time-derived rowids on a copy table + fts rebuild; subquery/two-step/plain
  P-B   — original rowids + time column + covering index + ephemeral set membership
  P-C   — P-A rowids with fts5 rowid-range windows (planner-independent descent)
  P-D   — time-ordered walk + per-rowid EXISTS probe on fts (no set materialization)
E3 rides along: detail=column / detail=none fts rebuilds + differential battery.

Prints a compact table; no files written outside scratch.db.
"""
import sqlite3, time, sys, os

DB = os.path.join(os.path.dirname(__file__), '..', '.fixtures', 'e1', 'scratch.db')
Q = '"the"'          # the load-test query: 58k/178k match set
LIMIT = 51           # limit+1 probe (contract uses limit+1 too)

def qtime(c, sql, params=(), reps=3):
    runs = []
    rows = 0
    for i in range(reps):
        t = time.perf_counter()
        try:
            cur = c.execute(sql, params) if params else c.execute(sql)
            out = cur.fetchall()
            runs.append(time.perf_counter() - t)
            rows = len(out)
        except sqlite3.Error as e:
            return None, str(e)
    return runs, rows

def eqp(c, sql, params=()):
    lines = []
    try:
        for r in c.execute('EXPLAIN QUERY PLAN ' + sql, params):
            lines.append(str(r[-1]))
    except sqlite3.Error as e:
        lines.append(f'ERR {e}')
    return ' | '.join(lines)

def main():
    if not os.path.exists(DB):
        sys.exit(f'missing {DB}')
    c = sqlite3.connect(DB)
    c.execute('PRAGMA journal_mode=WAL')

    print('== BASE (current search_parts shape) ==')
    base_sql = (
        "SELECT ps.session_id, ps.message_id, ps.part_id, m.role, m.time_created, ps.text "
        "FROM part_search ps JOIN msg m ON m.id = ps.message_id "
        "WHERE ps.rowid IN (SELECT rowid FROM part_search_fts WHERE part_search_fts MATCH ?1) "
        "ORDER BY m.time_created DESC, ps.rowid DESC LIMIT ?2 OFFSET 0")
    runs, rows = qtime(c, base_sql, (Q, LIMIT))
    print(f'  rows={rows} runs={[round(x,3) for x in runs] if runs else runs}')
    print(f'  EQP: {eqp(c, base_sql, (Q, LIMIT))}')
    match_n = c.execute('SELECT count(*) FROM part_search_fts WHERE part_search_fts MATCH ?', (Q,)).fetchone()[0]
    print(f'  match set: {match_n}')

    # ---- build P-A: time-ordered dense rowids + fts over copy table ----
    print('== building ps_t (time-ranked rowid) + fts ==')
    have = {r[0] for r in c.execute("SELECT name FROM sqlite_master WHERE name IN ('ps_t_fts')")}
    t0 = time.perf_counter()
    if have:
        print('  exists — skip build')
    else:
        c.executescript('''
      DROP TABLE IF EXISTS ps_t;
      DROP TABLE IF EXISTS ps_t_fts;
      CREATE TABLE ps_t (
        rowid INTEGER PRIMARY KEY,
        part_id TEXT NOT NULL,
        session_id TEXT NOT NULL,
        message_id TEXT NOT NULL,
        time_created INTEGER NOT NULL,
        text TEXT NOT NULL
      ) STRICT;
      INSERT INTO ps_t (rowid, part_id, session_id, message_id, time_created, text)
      SELECT ROW_NUMBER() OVER (ORDER BY m.time_created, ps.rowid),
             ps.part_id, ps.session_id, ps.message_id, m.time_created, ps.text
      FROM part_search ps JOIN msg m ON m.id = ps.message_id;
      CREATE VIRTUAL TABLE ps_t_fts USING fts5(text, content='ps_t', content_rowid='rowid', tokenize='trigram');
      INSERT INTO ps_t_fts(rowid, text) SELECT rowid, text FROM ps_t;
    ''')
        print(f'  build: {time.perf_counter()-t0:.1f}s')

    print('== P-A variants (rowid DESC walk on time-ranked fts) ==')
    pa_plain = 'SELECT rowid FROM ps_t_fts WHERE ps_t_fts MATCH ?1 ORDER BY rowid DESC LIMIT ?2'
    runs, rows = qtime(c, pa_plain, (Q, LIMIT))
    print(f'  PA-plain: rows={rows} runs={[round(x,4) for x in runs] if runs else runs}')
    print(f'    EQP: {eqp(c, pa_plain, (Q, LIMIT))}')

    pa_sub = (
        "SELECT ps.session_id, ps.message_id, ps.part_id, m.role, m.time_created, ps.text "
        "FROM ps_t ps JOIN msg m ON m.id = ps.message_id "
        "WHERE ps.rowid IN (SELECT rowid FROM ps_t_fts WHERE ps_t_fts MATCH ?1 ORDER BY rowid DESC LIMIT ?2) "
        "ORDER BY ps.rowid DESC")
    runs, rows = qtime(c, pa_sub, (Q, LIMIT))
    print(f'  PA-sub:   rows={rows} runs={[round(x,4) for x in runs] if runs else runs}')
    print(f'    EQP: {eqp(c, pa_sub, (Q, LIMIT))}')

    # two-step (Rust would do this): set of <=LIMIT rowids, then payload
    t0 = time.perf_counter()
    ids = [r[0] for r in c.execute(pa_plain, (Q, LIMIT))]
    t_ids = time.perf_counter() - t0
    ph = ','.join('?' * len(ids))
    payload_sql = (
        f"SELECT ps.session_id, ps.message_id, ps.part_id, m.role, m.time_created, ps.text "
        f"FROM ps_t ps JOIN msg m ON m.id = ps.message_id WHERE ps.rowid IN ({ph}) ORDER BY ps.rowid DESC")
    runs, rows = qtime(c, payload_sql, tuple(ids))
    print(f'  PA-two-step: ids={len(ids)} step1={t_ids*1000:.1f}ms step2_runs={[round(x*1000,1) for x in runs] if runs else runs}ms rows={rows}')

    # ---- P-B: time column + covering index + set membership (original rowids) ----
    print('== building ps_b (original rowid + time col + index) ==')
    have = {r[0] for r in c.execute("SELECT name FROM sqlite_master WHERE name IN ('ps_b_time_idx')")}
    t0 = time.perf_counter()
    if have:
        print('  exists — skip build')
    else:
        c.executescript('''
      DROP TABLE IF EXISTS ps_b;
      CREATE TABLE ps_b (
        rowid INTEGER PRIMARY KEY,
        part_id TEXT NOT NULL,
        session_id TEXT NOT NULL,
        message_id TEXT NOT NULL,
        time_created INTEGER NOT NULL,
        text TEXT NOT NULL
      ) STRICT;
      INSERT INTO ps_b (rowid, part_id, session_id, message_id, time_created, text)
      SELECT ps.rowid, ps.part_id, ps.session_id, ps.message_id, m.time_created, ps.text
      FROM part_search ps JOIN msg m ON m.id = ps.message_id;
      CREATE INDEX ps_b_time_idx ON ps_b(time_created DESC, rowid DESC);
    ''')
        print(f'  build: {time.perf_counter()-t0:.1f}s')
    pb = (
        "SELECT rowid FROM ps_b WHERE rowid IN "
        "(SELECT rowid FROM part_search_fts WHERE part_search_fts MATCH ?1) "
        "ORDER BY time_created DESC, rowid DESC LIMIT ?2")
    runs, rows = qtime(c, pb, (Q, LIMIT))
    print(f'  P-B: rows={rows} runs={[round(x,4) for x in runs] if runs else runs}')
    print(f'    EQP: {eqp(c, pb, (Q, LIMIT))}')

    # ---- P-C: rowid-range windows on P-A fts ----
    print('== P-C (rowid-range windows over time-ranked fts) ==')
    mx = c.execute('SELECT max(rowid) FROM ps_t').fetchone()[0]
    mn = c.execute('SELECT min(rowid) FROM ps_t').fetchone()[0]
    span = mx - mn + 1
    for frac, w in (('all', 0), ('top20%', int(span * 0.2)), ('top5%', int(span * 0.05)), ('top1%', int(span * 0.01))):
        if w == 0:
            sql = pa_plain
            params = (Q, LIMIT)
        else:
            sql = 'SELECT rowid FROM ps_t_fts WHERE ps_t_fts MATCH ?1 AND rowid >= ?2 ORDER BY rowid DESC LIMIT ?3'
            params = (Q, mx - w + 1, LIMIT)
        runs, rows = qtime(c, sql, params)
        print(f'  PC-{frac}: rows={rows} runs={[round(x,4) for x in runs] if runs else runs}')
        if frac == 'top5%':
            print(f'    EQP: {eqp(c, sql, params)}')

    # ---- P-D: time walk + EXISTS probe (no set materialization) ----
    print('== P-D (time-ordered walk + per-rowid EXISTS probe) ==')
    pd = (
        "SELECT ps.rowid FROM ps_b ps WHERE EXISTS "
        "(SELECT 1 FROM part_search_fts f WHERE f.rowid = ps.rowid AND part_search_fts MATCH ?1) "
        "ORDER BY ps.time_created DESC, ps.rowid DESC LIMIT ?2")
    runs, rows = qtime(c, pd, (Q, LIMIT))
    print(f'  P-D: rows={rows} runs={[round(x,4) for x in runs] if runs else runs}')
    print(f'    EQP: {eqp(c, pd, (Q, LIMIT))}')

    # ---- correctness: all shapes must agree with BASE ordering ----
    print('== order equivalence vs BASE (part_ids) ==')
    base = [r[2] for r in c.execute(base_sql, (Q, LIMIT))]
    for name, sql, params in (
        ('PA-sub', pa_sub, (Q, LIMIT)),
        ('P-B', pb, (Q, LIMIT)),
        ('P-D', pd, (Q, LIMIT)),
    ):
        try:
            if name == 'PA-sub':
                got = [r[2] for r in c.execute(sql, params)]
            elif name == 'P-B':
                # ps_b.rowid == part_search.rowid => map to part_id
                ids = [r[0] for r in c.execute(sql, params)]
                got = [r[0] for r in c.execute(f"SELECT part_id FROM ps_b WHERE rowid IN ({','.join('?'*len(ids))})", ids)] if ids else []
                # order differs (set) — compare as sets for P-B
                print(f'  {name}: set_equal={set(got)==set(base)} (order checked separately)')
                continue
            else:
                ids = [r[0] for r in c.execute(sql, params)]
                got = [r[0] for r in c.execute(f"SELECT part_id FROM ps_b WHERE rowid IN ({','.join('?'*len(ids))})", ids)] if ids else []
                print(f'  {name}: set_equal={set(got)==set(base)}')
                continue
            print(f'  {name}: ordered_equal={got==base}')
        except sqlite3.Error as e:
            print(f'  {name}: ERR {e}')

    # ---- E3: detail= column/none differential battery ----
    print('== E3 (detail= differential battery) ==')
    have = {r[0] for r in c.execute("SELECT name FROM sqlite_master WHERE name IN ('ps_dnone')")}
    t0 = time.perf_counter()
    if have:
        print('  exists — skip rebuild')
    else:
        c.executescript('''
          CREATE VIRTUAL TABLE IF NOT EXISTS ps_dnone USING fts5(text, content='part_search', content_rowid='rowid', tokenize='trigram', detail=none);
          INSERT INTO ps_dnone(rowid, text) SELECT rowid, text FROM part_search WHERE rowid NOT IN (SELECT rowid FROM ps_dnone);
        ''')
        print(f'  detail=none rebuild: {time.perf_counter()-t0:.1f}s')
    try:
        sz = os.path.getsize(DB)
        print(f'  scratch.db size now: {sz/1e6:.0f} MB')
    except OSError:
        pass
    battery = ['the', 'and', 'error', 'panic', 'OpenCode', 'error in', 'not found',
               'session', 'thread', 'failed', 'retry', '"error in"', 'over 100',
               'buffer', 'index', 'cache', 'refine', 'stream', 'a', 'zz', 'e']
    diffs = 0
    for term in battery:
        needle = f'"{term}"'
        try:
            full = {r[0] for r in c.execute('SELECT rowid FROM part_search_fts WHERE part_search_fts MATCH ?', (needle,))}
            col = {r[0] for r in c.execute('SELECT rowid FROM ps_dcol WHERE ps_dcol MATCH ?', (needle,))}
            none = {r[0] for r in c.execute('SELECT rowid FROM ps_dnone WHERE ps_dnone MATCH ?', (needle,))}
            same = (full == col == none)
            if not same:
                diffs += 1
                print(f'  DIFF term={term!r}: full={len(full)} col={len(col)} none={len(none)}')
            else:
                print(f'  ok term={term!r}: n={len(full)}')
        except sqlite3.Error as e:
            diffs += 1
            print(f'  ERR term={term!r}: {e}')
    print(f'  E3 verdict inputs: {diffs} diffs out of {len(battery)}')
    c.close()

if __name__ == '__main__':
    main()
