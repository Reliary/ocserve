//! Schema: STRICT tables, fixed-size metadata only, user_version gate.
//! Payloads never live here — they live in the blob store (STORAGE.md §4).

use anyhow::{Result, bail};
use rusqlite::Connection;

/// Bump when the schema changes; refuse to open mismatches with an actionable error
/// (reliary8/stria pattern: schema.rs user_version gate).
pub const SCHEMA_VERSION: i64 = 10;

const DDL: &str = "
-- session metadata (no payloads)
CREATE TABLE session (
    id            TEXT PRIMARY KEY,
    project_id    TEXT NOT NULL DEFAULT 'global',
    slug          TEXT NOT NULL DEFAULT '',
    directory     TEXT NOT NULL DEFAULT '',
    parent_id     TEXT,
    title         TEXT NOT NULL DEFAULT '',
    version       TEXT NOT NULL DEFAULT '1',
    path          TEXT NOT NULL DEFAULT '',
    agent         TEXT,
    model         TEXT,
    cost          REAL NOT NULL DEFAULT 0,
    summary_additions INTEGER NOT NULL DEFAULT 0,
    summary_deletions INTEGER NOT NULL DEFAULT 0,
    summary_files     INTEGER NOT NULL DEFAULT 0,
    tokens_input  INTEGER NOT NULL DEFAULT 0,
    tokens_output INTEGER NOT NULL DEFAULT 0,
    tokens_reasoning INTEGER NOT NULL DEFAULT 0,
    tokens_cache_read INTEGER NOT NULL DEFAULT 0,
    tokens_cache_write INTEGER NOT NULL DEFAULT 0,
    time_created  INTEGER NOT NULL,
    time_updated  INTEGER NOT NULL,
    version_dirt  INTEGER NOT NULL DEFAULT 0,
    permission    TEXT
) STRICT;

CREATE INDEX idx_session_updated ON session(time_updated DESC);
CREATE INDEX idx_session_parent ON session(parent_id) WHERE parent_id IS NOT NULL;

-- messages: full wire info as JSON (small, ≤1KB typical); ordered by seq
CREATE TABLE msg (
    id          TEXT PRIMARY KEY,
    session_id  TEXT NOT NULL REFERENCES session(id) ON DELETE CASCADE,
    role        TEXT NOT NULL,
    seq         INTEGER NOT NULL,
    time_created INTEGER NOT NULL,
    info        TEXT NOT NULL
) STRICT;

CREATE INDEX idx_msg_session ON msg(session_id, seq);
CREATE INDEX idx_msg_page ON msg(session_id, time_created, id);

-- parts: small payloads inline (≤8KB per MEMORY §6), large → blob store
CREATE TABLE msg_part (
    id          TEXT PRIMARY KEY,
    message_id  TEXT NOT NULL REFERENCES msg(id) ON DELETE CASCADE,
    session_id  TEXT NOT NULL,
    seq         INTEGER NOT NULL,
    type        TEXT NOT NULL,
    byte_len    INTEGER NOT NULL,
    inline      TEXT,
    blob_sha    TEXT
) STRICT;

CREATE INDEX idx_part_msg ON msg_part(message_id, seq);
CREATE INDEX idx_part_session ON msg_part(session_id, seq);

-- bounded event ring (never RAM; never unbounded: PLAN §5)
CREATE TABLE event (
    seq         INTEGER PRIMARY KEY AUTOINCREMENT,
    session_id  TEXT,
    project_id  TEXT NOT NULL DEFAULT 'global',
    type        TEXT NOT NULL,
    payload     TEXT NOT NULL,
    time_created INTEGER NOT NULL
) STRICT;

CREATE INDEX idx_event_session ON event(session_id, seq);

-- permission decisions (per project+action+resource, S8)
CREATE TABLE permission (
    project_id  TEXT NOT NULL,
    action      TEXT NOT NULL,
    resource    TEXT NOT NULL,
    decision    TEXT NOT NULL,
    time_created INTEGER NOT NULL,
    PRIMARY KEY (project_id, action, resource)
) STRICT;

-- todos
CREATE TABLE todo (
    id          TEXT PRIMARY KEY,
    session_id  TEXT NOT NULL REFERENCES session(id) ON DELETE CASCADE,
    content     TEXT NOT NULL,
    status      TEXT NOT NULL,
    priority    TEXT,
    time_created INTEGER NOT NULL,
    time_updated INTEGER NOT NULL
) STRICT;

CREATE INDEX idx_todo_session ON todo(session_id);

-- FTS5 external-content projection, MAIN DB only (STORAGE.md §1 — ATTACH impossible)
-- legacy-delta sync state (dev bridge: legacy -> refine additive pulls;
-- dropped relevance once the final migration lands — PLAN §17)
CREATE TABLE import_sync (
    session_id TEXT PRIMARY KEY,
    cursor TEXT,
    last_sync_ms INTEGER NOT NULL DEFAULT 0
) STRICT;

-- content search over part payloads (trigram = substring semantics >=3
-- chars — W1; LIKE fallback for shorter needles at the query layer).
-- text holds the UNCOMPRESSED stored JSON even for blobbed parts, so
-- zstd payloads stay searchable. ad/au triggers keep the FTS shadow in
-- lockstep; FK cascade fires them on session/msg/part deletion (proven:
-- child triggers fire on FK cascade with recursive_triggers=OFF).
-- Rowid contract (PERF-10X S-A, v10): rowid = time_created * 1048576 + slot
-- so ORDER BY rowid == the search contract order (time DESC, insert order
-- within the ms) and the fts walk early-terminates at LIMIT instead of
-- materializing the corpus match set (E1: 5.9s -> 0.2ms warm). Allocated by
-- part_search_upsert_ops (max+1 within the ms range; % 1048576 makes a
-- pathological overflow a loud PK conflict). message_id FK makes a missing
-- msg impossible (rowid expr would otherwise be NULL -> auto-assign).
CREATE TABLE part_search (
    rowid      INTEGER PRIMARY KEY,
    part_id    TEXT NOT NULL UNIQUE REFERENCES msg_part(id) ON DELETE CASCADE,
    session_id TEXT NOT NULL,
    message_id TEXT NOT NULL REFERENCES msg(id) ON DELETE CASCADE,
    text       TEXT NOT NULL
) STRICT;

CREATE INDEX idx_part_search_session ON part_search(session_id);

CREATE VIRTUAL TABLE part_search_fts USING fts5(
    text, content='part_search', content_rowid='rowid', tokenize='trigram'
);

CREATE TRIGGER part_search_ai AFTER INSERT ON part_search BEGIN
    INSERT INTO part_search_fts(rowid, text) VALUES (new.rowid, new.text);
END;
CREATE TRIGGER part_search_rt AFTER INSERT ON part_search
WHEN (NEW.rowid / 1048576) <> (SELECT time_created FROM msg WHERE id = NEW.message_id)
BEGIN
    SELECT RAISE(ABORT, 'part_search rowid/time mismatch (encoding overflow or direct rowid write)');
END;
CREATE TRIGGER part_search_ad AFTER DELETE ON part_search BEGIN
    INSERT INTO part_search_fts(part_search_fts, rowid, text)
    VALUES ('delete', old.rowid, old.text);
END;
CREATE TRIGGER part_search_au AFTER UPDATE ON part_search BEGIN
    INSERT INTO part_search_fts(part_search_fts, rowid, text)
    VALUES ('delete', old.rowid, old.text);
    INSERT INTO part_search_fts(rowid, text) VALUES (new.rowid, new.text);
END;

-- blob chunk index: content-addressed, fixed-size rows (STORAGE.md §4)
CREATE TABLE blob_chunk (
    sha       TEXT NOT NULL,
    ord       INTEGER NOT NULL,
    byte_len  INTEGER NOT NULL,
    PRIMARY KEY (sha, ord)
) STRICT, WITHOUT ROWID;

-- blob objects: total length + codec, GC bookkeeping
CREATE TABLE blob_object (
    sha        TEXT PRIMARY KEY,
    byte_len   INTEGER NOT NULL,
    chunk_cnt  INTEGER NOT NULL,
    codec      TEXT NOT NULL DEFAULT 'zstd',
    created_at INTEGER NOT NULL,
    deleted_at INTEGER
) STRICT;

CREATE TABLE import_manifest (
    key TEXT PRIMARY KEY,
    value TEXT NOT NULL
) STRICT;

-- compaction projection (M6): one row per compaction anchor part — assembly
-- needs the LAST anchor + tail BEFORE a forward streaming pass (COMPACTION
-- §5 P1); maintained by the part helpers exactly like part_search.
CREATE TABLE compaction (
    session_id     TEXT NOT NULL,
    part_id        TEXT NOT NULL UNIQUE REFERENCES msg_part(id) ON DELETE CASCADE,
    user_msg_id    TEXT NOT NULL REFERENCES msg(id) ON DELETE CASCADE,
    auto           INTEGER NOT NULL DEFAULT 0,
    overflow       INTEGER NOT NULL DEFAULT 0,
    tail_start_id  TEXT,
    summary_msg_id TEXT REFERENCES msg(id) ON DELETE SET NULL,
    time_ms        INTEGER NOT NULL DEFAULT 0
) STRICT;
CREATE INDEX idx_compaction_session ON compaction(session_id, time_ms);
";

/// Apply schema to a fresh DB and stamp user_version. Handles 0→current,
/// 1→current (session columns + message tables), 2→3 (message tables);
/// refuses anything else.
pub fn migrate(conn: &Connection) -> Result<()> {
    let ver: i64 = conn
        .query_row("PRAGMA user_version", [], |r| r.get(0))
        .expect("user_version readable");
    if ver == SCHEMA_VERSION {
        return Ok(());
    }
    // Step loop (fixes a latent flaw: the old one-shot match stamped
    // user_version = SCHEMA even when intermediate arms were skipped, so a
    // v1/v2 database would silently miss later steps' DDL). Fresh DBs (0)
    // get the current DDL and jump straight to SCHEMA_VERSION; every other
    // version advances exactly one step per pass.
    loop {
        let ver: i64 = conn.pragma_query_value(None, "user_version", |r| r.get(0))?;
        if ver == SCHEMA_VERSION {
            break;
        }
        match ver {
            0 => {
                conn.execute_batch(DDL)?;
                conn.pragma_update(None, "user_version", SCHEMA_VERSION)?;
                break;
            }
            1 => {
                // v1→v2: session list fields (PLAN F1 — TUI/oc-remote list shape).
                conn.execute_batch(
                    "ALTER TABLE session ADD COLUMN path TEXT NOT NULL DEFAULT '';
                 ALTER TABLE session ADD COLUMN agent TEXT;
                 ALTER TABLE session ADD COLUMN model TEXT;
                 ALTER TABLE session ADD COLUMN cost REAL NOT NULL DEFAULT 0;
                 ALTER TABLE session ADD COLUMN summary_additions INTEGER NOT NULL DEFAULT 0;
                 ALTER TABLE session ADD COLUMN summary_deletions INTEGER NOT NULL DEFAULT 0;
                 ALTER TABLE session ADD COLUMN summary_files INTEGER NOT NULL DEFAULT 0;
                 ALTER TABLE session ADD COLUMN tokens_input INTEGER NOT NULL DEFAULT 0;
                 ALTER TABLE session ADD COLUMN tokens_output INTEGER NOT NULL DEFAULT 0;
                 ALTER TABLE session ADD COLUMN tokens_reasoning INTEGER NOT NULL DEFAULT 0;
                 ALTER TABLE session ADD COLUMN tokens_cache_read INTEGER NOT NULL DEFAULT 0;
                 ALTER TABLE session ADD COLUMN tokens_cache_write INTEGER NOT NULL DEFAULT 0;",
                )?;
                migrate_message_tables(conn)?;
            }
            2 => {
                migrate_message_tables(conn)?;
            }
            3 => {
                // v3→v4: session-scoped part index (M3 latency gate: count(*) on
                // the 16k-msg session was an 86ms table scan without it).
                conn.execute_batch(
                    "CREATE INDEX IF NOT EXISTS idx_part_session ON msg_part(session_id, seq);",
                )?;
            }
            4 => {
                // v4→v5: cursor-paging window index (session_id, time_created, id)
                // — tuple-ordered before/limit pages without scanning the session.
                conn.execute_batch(
                    "CREATE INDEX IF NOT EXISTS idx_msg_page ON msg(session_id, time_created, id);",
                )?;
            }
            5 => {
                // v5→v6: legacy-delta sync state (additive bridge; see sync.rs)
                conn.execute_batch(
                    "CREATE TABLE IF NOT EXISTS import_sync (
                    session_id TEXT PRIMARY KEY,
                    cursor TEXT,
                    last_sync_ms INTEGER NOT NULL DEFAULT 0
                ) STRICT;",
                )?;
            }
            6 => {
                // v6→v7: part content search (W1). Trigram stack + drop the
                // dead title tables (search_doc/search_fts never populated —
                // count=0 live; title search runs in-memory contains).
                // Backfill runs at boot (store::backfill_part_search).
                conn.execute_batch(
                    "CREATE TABLE part_search (
                        rowid      INTEGER PRIMARY KEY,
                        part_id    TEXT NOT NULL UNIQUE REFERENCES msg_part(id) ON DELETE CASCADE,
                        session_id TEXT NOT NULL,
                        message_id TEXT NOT NULL,
                        text       TEXT NOT NULL
                    ) STRICT;
                    CREATE VIRTUAL TABLE part_search_fts USING fts5(
                        text, content='part_search', content_rowid='rowid', tokenize='trigram'
                    );
                    CREATE TRIGGER part_search_ai AFTER INSERT ON part_search BEGIN
                        INSERT INTO part_search_fts(rowid, text) VALUES (new.rowid, new.text);
                    END;
                    CREATE TRIGGER part_search_ad AFTER DELETE ON part_search BEGIN
                        INSERT INTO part_search_fts(part_search_fts, rowid, text)
                        VALUES ('delete', old.rowid, old.text);
                    END;
                    CREATE TRIGGER part_search_au AFTER UPDATE ON part_search BEGIN
                        INSERT INTO part_search_fts(part_search_fts, rowid, text)
                        VALUES ('delete', old.rowid, old.text);
                        INSERT INTO part_search_fts(rowid, text) VALUES (new.rowid, new.text);
                    END;
                    DROP TABLE IF EXISTS search_fts;
                    DROP TABLE IF EXISTS search_doc;",
                )?;
            }
            7 => {
                // v7→v8: compaction projection (M6, COMPACTION.md §4).
                // Backfill of pre-existing anchors runs at boot
                // (store::backfill_compaction).
                conn.execute_batch(
                    "CREATE TABLE compaction (
                        session_id     TEXT NOT NULL,
                        part_id        TEXT NOT NULL UNIQUE REFERENCES msg_part(id) ON DELETE CASCADE,
                        user_msg_id    TEXT NOT NULL REFERENCES msg(id) ON DELETE CASCADE,
                        auto           INTEGER NOT NULL DEFAULT 0,
                        overflow       INTEGER NOT NULL DEFAULT 0,
                        tail_start_id  TEXT,
                        summary_msg_id TEXT REFERENCES msg(id) ON DELETE SET NULL,
                        time_ms        INTEGER NOT NULL DEFAULT 0
                    ) STRICT;
                    CREATE INDEX idx_compaction_session ON compaction(session_id, time_ms);",
                )?;
            }
            8 => {
                // v8→v9: persisted per-session "always" permission keys
                // (K-ALWAYS — was in-memory only; lost on restart).
                conn.execute_batch("ALTER TABLE session ADD COLUMN permission TEXT;")?;
            }
            9 => {
                // v9->v10: time-encoded part_search rowids (PERF-10X S-A).
                // Rowid reorder cannot happen in place (PK collisions mid-
                // shuffle) — full rebuild in one transaction: copy with dense
                // per-ms ranks, swap, rebuild the fts shadow, recreate the
                // triggers (dropping the table kills them), add the session
                // index and the rowid/time invariant trigger.
                let step = |label: &str, sql: &str| -> Result<()> {
                    let t = std::time::Instant::now();
                    conn.execute_batch(sql)?;
                    tracing::info!(
                        "migrate v9->v10: {label} took {:.1}s",
                        t.elapsed().as_secs_f32()
                    );
                    Ok(())
                };
                let (rows, bytes): (i64, i64) = conn.query_row(
                    "SELECT count(*), coalesce(sum(length(text)), 0) FROM part_search",
                    [],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )?;
                tracing::info!(
                    "migrate v9->v10: time-encoded part_search rowids — {rows} rows, {bytes} bytes text; the trigram fts rebuild is the long step (measured ~5-9 min on a 1.5GB db, one-time; fresh imports skip this path)"
                );
                conn.execute_batch("BEGIN;")?;
                step(
                    "create+copy",
                    "CREATE TABLE part_search_v10 (
                        rowid      INTEGER PRIMARY KEY,
                        part_id    TEXT NOT NULL UNIQUE REFERENCES msg_part(id) ON DELETE CASCADE,
                        session_id TEXT NOT NULL,
                        message_id TEXT NOT NULL REFERENCES msg(id) ON DELETE CASCADE,
                        text       TEXT NOT NULL
                    ) STRICT;
                    INSERT INTO part_search_v10 (rowid, part_id, session_id, message_id, text)
                    SELECT m.time_created * 1048576
                         + (ROW_NUMBER() OVER (PARTITION BY m.time_created ORDER BY ps.rowid) - 1),
                           ps.part_id, ps.session_id, ps.message_id, ps.text
                    FROM part_search ps JOIN msg m ON m.id = ps.message_id;",
                )?;
                step(
                    "drop old+swap",
                    "DROP TABLE part_search_fts;
                    DROP TABLE part_search;
                    ALTER TABLE part_search_v10 RENAME TO part_search;
                    CREATE INDEX idx_part_search_session ON part_search(session_id);",
                )?;
                step(
                    "create fts + rebuild (long)",
                    "CREATE VIRTUAL TABLE part_search_fts USING fts5(
                        text, content='part_search', content_rowid='rowid', tokenize='trigram'
                    );
                    INSERT INTO part_search_fts(part_search_fts) VALUES('rebuild');",
                )?;
                step(
                    "triggers",
                    "CREATE TRIGGER part_search_ai AFTER INSERT ON part_search BEGIN
                        INSERT INTO part_search_fts(rowid, text) VALUES (new.rowid, new.text);
                    END;
                    CREATE TRIGGER part_search_ad AFTER DELETE ON part_search BEGIN
                        INSERT INTO part_search_fts(part_search_fts, rowid, text)
                        VALUES ('delete', old.rowid, old.text);
                    END;
                    CREATE TRIGGER part_search_au AFTER UPDATE ON part_search BEGIN
                        INSERT INTO part_search_fts(part_search_fts, rowid, text)
                        VALUES ('delete', old.rowid, old.text);
                        INSERT INTO part_search_fts(rowid, text) VALUES (new.rowid, new.text);
                    END;
                    CREATE TRIGGER part_search_rt AFTER INSERT ON part_search
                    WHEN (NEW.rowid / 1048576) <> (SELECT time_created FROM msg WHERE id = NEW.message_id)
                    BEGIN
                        SELECT RAISE(ABORT, 'part_search rowid/time mismatch (encoding overflow or direct rowid write)');
                    END;",
                )?;
                conn.execute_batch("COMMIT")?;
                tracing::info!("migrate v9->v10: committed");
            }
            other => {
                bail!(
                    "database schema version {other} out of step for {SCHEMA_VERSION}; \
                 refuse to touch it (upgrade path runs through migrations)"
                );
            }
        }
        let now: i64 = conn.pragma_query_value(None, "user_version", |r| r.get(0))?;
        conn.pragma_update(None, "user_version", now + 1)?;
    }
    Ok(())
}

/// v2→v3 message/part → msg/msg_part wire-shape migration (empty-table safe).
fn migrate_message_tables(conn: &Connection) -> Result<()> {
    let has_legacy: bool = conn
        .query_row(
            "SELECT count(*) > 0 FROM sqlite_master \
             WHERE type='table' AND name IN ('message','part')",
            [],
            |r| r.get(0),
        )
        .unwrap_or(false);
    if has_legacy {
        let msg_count: i64 = conn
            .query_row("SELECT count(*) FROM message", [], |r| r.get(0))
            .unwrap_or(0);
        let part_count: i64 = conn
            .query_row("SELECT count(*) FROM part", [], |r| r.get(0))
            .unwrap_or(0);
        if msg_count > 0 || part_count > 0 {
            bail!(
                "message-table migration refuses to drop {msg_count} messages/{part_count} parts; \
                 re-import instead"
            );
        }
        conn.execute_batch("DROP TABLE part; DROP TABLE message;")?;
    }
    // idempotent: skip create if msg already exists (partial migration)
    let has_new: bool = conn
        .query_row(
            "SELECT count(*) > 0 FROM sqlite_master WHERE type='table' AND name='msg'",
            [],
            |r| r.get(0),
        )
        .unwrap_or(false);
    if has_new {
        return Ok(());
    }
    conn.execute_batch(
        "CREATE TABLE msg (
            id TEXT PRIMARY KEY,
            session_id TEXT NOT NULL REFERENCES session(id) ON DELETE CASCADE,
            role TEXT NOT NULL,
            seq INTEGER NOT NULL,
            time_created INTEGER NOT NULL,
            info TEXT NOT NULL
        ) STRICT;
        CREATE INDEX idx_msg_session ON msg(session_id, seq);
        CREATE INDEX idx_msg_page ON msg(session_id, time_created, id);
        CREATE TABLE msg_part (
            id TEXT PRIMARY KEY,
            message_id TEXT NOT NULL REFERENCES msg(id) ON DELETE CASCADE,
            session_id TEXT NOT NULL,
            seq INTEGER NOT NULL,
            type TEXT NOT NULL,
            byte_len INTEGER NOT NULL,
            inline TEXT,
            blob_sha TEXT
        ) STRICT;
        CREATE INDEX idx_part_msg ON msg_part(message_id, seq);
CREATE INDEX idx_part_session ON msg_part(session_id, seq);",
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pragma;

    #[test]
    fn migrate_is_idempotent_and_strict() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("t.db");
        let conn = pragma::create_new(&p).unwrap();
        migrate(&conn).unwrap();
        migrate(&conn).unwrap(); // second run: no-op
        // STRICT: wrong-typed write must fail
        let err = conn.execute(
            "INSERT INTO session (id, time_created, time_updated) VALUES (?, ?, ?)",
            rusqlite::params!["s1", "not-an-int", 1],
        );
        assert!(err.is_err(), "STRICT table must reject text in INTEGER");
        // index presence for the hot list query
        let plans: String = conn
            .query_row(
                "EXPLAIN QUERY PLAN SELECT * FROM session ORDER BY time_updated DESC",
                [],
                |r| r.get(3),
            )
            .unwrap();
        assert!(plans.contains("idx_session_updated"), "got plan: {plans}");
    }

    #[test]
    fn version_mismatch_refuses() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("t.db");
        let conn = pragma::create_new(&p).unwrap();
        migrate(&conn).unwrap();
        conn.pragma_update(None, "user_version", 99).unwrap();
        let err = migrate(&conn).unwrap_err();
        assert!(
            err.to_string().contains("99"),
            "message must name the version"
        );
    }
}

#[cfg(test)]
mod fts_m0 {
    use super::*;
    use crate::pragma;

    /// W1 lifecycle: trigram substring search over the part content stack —
    /// fresh-create shape, mid-word match, update reindex, FK-cascade cleanup
    /// (child triggers fire on FK cascade — proven live-in-RAM before build).
    #[test]
    fn part_search_lifecycle_trigram() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("t.db");
        let conn = pragma::create_new(&p).unwrap();
        migrate(&conn).unwrap();
        let has: i64 = conn
            .query_row(
                "SELECT count(*) FROM sqlite_master WHERE name IN ('part_search','part_search_fts')",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(has, 2, "trigram stack on fresh create");
        let dead: i64 = conn
            .query_row(
                "SELECT count(*) FROM sqlite_master WHERE name IN ('search_doc','search_fts')",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(dead, 0, "dead title-search tables dropped");
        conn.execute_batch(
            "INSERT INTO session (id, project_id, directory, path, slug, title, version, time_created, time_updated) VALUES ('ses_1','global','/w','s','s','t','1',1,1);
             INSERT INTO msg (id, session_id, role, seq, time_created, info) VALUES ('m1','ses_1','user',1,1,'{}');
             INSERT INTO msg_part (id, message_id, session_id, seq, type, byte_len, inline, blob_sha)
             VALUES ('p1','m1','ses_1',1,'text',44,'{}',NULL);",
        )
        .unwrap();
        // v10 rowid contract: time_created(1) * 1048576 + slot(0) — the
        // part_search_rt trigger rejects anything else (proven: raw auto
        // rowids fail this test with "rowid/time mismatch").
        conn.execute(
            "INSERT INTO part_search (rowid, part_id, session_id, message_id, text)
             VALUES (1048576,'p1','ses_1','m1','{\"type\":\"text\",\"text\":\"the REFINE engine is fast\"}')",
            [],
        )
        .unwrap();
        let n: i64 = conn
            .query_row(
                "SELECT count(*) FROM part_search_fts WHERE part_search_fts MATCH '\"efin\"'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(n, 1, "mid-word substring match");
        conn.execute(
            "UPDATE part_search SET text='completely different zebra payload' WHERE part_id='p1'",
            [],
        )
        .unwrap();
        assert_eq!(
            conn.query_row(
                "SELECT count(*) FROM part_search_fts WHERE part_search_fts MATCH '\"zebra\"'",
                [],
                |r| r.get::<_, i64>(0)
            )
            .unwrap(),
            1
        );
        assert_eq!(
            conn.query_row(
                "SELECT count(*) FROM part_search_fts WHERE part_search_fts MATCH '\"efin\"'",
                [],
                |r| r.get::<_, i64>(0)
            )
            .unwrap(),
            0,
            "update reindexed"
        );
        conn.execute("DELETE FROM msg WHERE id='m1'", []).unwrap();
        assert_eq!(
            conn.query_row("SELECT count(*) FROM part_search", [], |r| r
                .get::<_, i64>(0))
                .unwrap(),
            0,
            "FK cascade cleaned part_search (ad trigger path)"
        );
    }

    /// M0 experiment: contentless FTS5 (`content=''`) behavior on the BUNDLED build.
    /// The system sqlite 3.53.0 on this box fails to even construct it (vtable
    /// constructor failed). If bundled behaves the same, any contentless design
    /// is off the table; if it works, it's still only usable inside the main DB
    /// (ATTACH separately falsified). The test records which world we're in.
    #[test]
    fn contentless_fts_status_recorded() {
        let conn = Connection::open_in_memory().unwrap();
        let attempt = conn.execute_batch(
            "CREATE VIRTUAL TABLE cl USING fts5(content='', tokenize='unicode61');
             INSERT INTO cl(rowid, cl) VALUES(1, 'hello world');
             SELECT count(*) FROM cl WHERE cl MATCH 'hello';",
        );
        match &attempt {
            Ok(()) => {
                // println (not tracing): test binary has no subscriber
                println!(
                    "M0-FTS-CONTENTLESS: works on bundled {}",
                    rusqlite::version()
                );
            }
            Err(e) => {
                // Not a failure of refine: our schema never uses contentless.
                // Recorded so STORAGE.md §1 stays honest about the build.
                println!(
                    "M0-FTS-CONTENTLESS: unavailable on bundled {}: {e} (design does not rely on it)",
                    rusqlite::version()
                );
            }
        }
        // The design we DO use (external-content over a local projection) must
        // always construct, regardless of contentless availability:
        conn.execute_batch(
            "CREATE TABLE doc(id INTEGER PRIMARY KEY, t TEXT);
             CREATE VIRTUAL TABLE ft USING fts5(content='doc', content_rowid='id', t);",
        )
        .expect("external-content local must construct on bundled build");
        // If contentless works, its full lifecycle must too — otherwise record it:
        if attempt.is_ok() {
            conn.execute_batch("SELECT count(*) FROM cl WHERE cl MATCH 'hello';")
                .expect("contentless roundtrip");
        }
    }
}

#[cfg(test)]
mod migration_tests {
    use super::*;
    use crate::pragma;

    /// v1→v2 ALTER path: old DBs gain list columns without data loss.
    #[test]
    fn migrates_v1_to_v2_preserving_rows() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("t.db");
        let conn = pragma::create_new(&p).unwrap();
        // build a v1-shaped DB manually
        conn.execute_batch(
            "CREATE TABLE session (
                id TEXT PRIMARY KEY, project_id TEXT NOT NULL DEFAULT 'global',
                slug TEXT NOT NULL DEFAULT '', directory TEXT NOT NULL DEFAULT '',
                parent_id TEXT, title TEXT NOT NULL DEFAULT '', version TEXT NOT NULL DEFAULT '1',
                time_created INTEGER NOT NULL, time_updated INTEGER NOT NULL,
                version_dirt INTEGER NOT NULL DEFAULT 0
            ) STRICT;
            INSERT INTO session (id, title, time_created, time_updated)
            VALUES ('ses_old', 'from v1', 100, 200);",
        )
        .unwrap();
        conn.pragma_update(None, "user_version", 1).unwrap();

        migrate(&conn).unwrap();
        let ver: i64 = conn
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(ver, SCHEMA_VERSION);
        let (title, cost, tok): (String, f64, i64) = conn
            .query_row(
                "SELECT title, cost, tokens_input FROM session WHERE id='ses_old'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap();
        assert_eq!(title, "from v1");
        assert_eq!(cost, 0.0);
        assert_eq!(tok, 0);
        // new columns exist and accept values
        conn.execute(
            "UPDATE session SET agent='build', model='{}', cost=1.5, tokens_input=42 WHERE id='ses_old'",
            [],
        )
        .unwrap();
        migrate(&conn).unwrap(); // idempotent at v2
    }
}

#[cfg(test)]
mod sa_search {
    //! PERF-10X S-A: time-encoded rowids + fts early-termination rewrite.
    use super::*;
    use crate::SEARCH_MATCH_GLOBAL;
    use crate::{apply_ops, part_search_upsert_ops, pragma, search_parts};

    const SLOTS: i64 = 1 << 20;

    fn seed(conn: &Connection) {
        conn.execute_batch(
            "INSERT INTO session (id, project_id, directory, path, slug, title, version, time_created, time_updated) VALUES ('ses_1','global','/w','s','s','t','1',1,1);
             INSERT INTO msg (id, session_id, role, seq, time_created, info) VALUES ('m1','ses_1','user',1,100,'{}');
             INSERT INTO msg (id, session_id, role, seq, time_created, info) VALUES ('m2','ses_1','user',2,500,'{}');
             INSERT INTO msg (id, session_id, role, seq, time_created, info) VALUES ('m3','ses_1','user',3,200,'{}');
             INSERT INTO msg_part (id, message_id, session_id, seq, type, byte_len, inline, blob_sha) VALUES ('p1','m1','ses_1',1,'text',5,'{}',NULL);
             INSERT INTO msg_part (id, message_id, session_id, seq, type, byte_len, inline, blob_sha) VALUES ('p2','m2','ses_1',1,'text',5,'{}',NULL);
             INSERT INTO msg_part (id, message_id, session_id, seq, type, byte_len, inline, blob_sha) VALUES ('p3','m3','ses_1',1,'text',5,'{}',NULL);",
        )
        .unwrap();
    }

    fn upsert(conn: &Connection, part: &str, msg: &str, text: &str) {
        apply_ops(conn, &[part_search_upsert_ops(part, "ses_1", msg, text)]).unwrap();
    }

    /// Rowid = time*SLOTS + slot: same-ms increments, cross-ms disjoint,
    /// delete-gaps never collide (max+1 allocation), rt trigger enforces.
    #[test]
    fn rowid_encoding_contract() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("t.db");
        let conn = pragma::create_new(&p).unwrap();
        migrate(&conn).unwrap();
        seed(&conn);
        upsert(&conn, "p1", "m1", "needle alpha");
        upsert(&conn, "p2", "m2", "needle beta");
        upsert(&conn, "p3", "m3", "needle gamma");
        let rows: Vec<(String, i64)> = conn
            .prepare("SELECT part_id, rowid FROM part_search ORDER BY rowid")
            .unwrap()
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap()
            .filter_map(|r| r.ok())
            .collect();
        assert_eq!(
            rows,
            vec![
                ("p1".into(), 100 * SLOTS + 0),
                ("p3".into(), 200 * SLOTS + 0),
                ("p2".into(), 500 * SLOTS + 0),
            ],
            "rowid order == time order (insert order differs: p1,p2,p3)"
        );
        // gap: delete p3 (slot0 of its ms), reinsert another part in m3's ms
        conn.execute("DELETE FROM part_search WHERE part_id='p3'", [])
            .unwrap();
        conn.execute(
            "INSERT INTO msg_part (id, message_id, session_id, seq, type, byte_len, inline, blob_sha) VALUES ('p4','m3','ses_1',2,'text',5,'{}',NULL)",
            [],
        )
        .unwrap();
        upsert(&conn, "p4", "m3", "needle delta");
        let r: i64 = conn
            .query_row(
                "SELECT rowid FROM part_search WHERE part_id='p4'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        // max in range was 500's? no — m3 range only had slot0 (deleted) →
        // coalesce(max) over empty range = -1 → slot0. Re-insert lands slot0
        // because the range is empty again; either way it must be IN range.
        assert_eq!(r, 200 * SLOTS + 0, "empty-range realloc uses slot0");
        // invariant trigger: raw wrong-rowid insert is rejected loudly
        conn.execute(
            "INSERT INTO msg_part (id, message_id, session_id, seq, type, byte_len, inline, blob_sha) VALUES ('p9','m1','ses_1',9,'text',5,'{}',NULL)",
            [],
        )
        .unwrap();
        let err = conn.execute(
            "INSERT INTO part_search (rowid, part_id, session_id, message_id, text) VALUES (77,'p9','ses_1','m1','x')",
            [],
        );
        assert!(
            err.unwrap_err().to_string().contains("rowid/time mismatch"),
            "rt trigger must reject rowid outside the message's ms range"
        );
    }

    /// Manual: time the v9→v10 rebuild on a REAL (copied) db.
    /// REFINE_SA_TIMING_DB=/path/to/scratch.db cargo test -p refine-store \
    ///   sa_search::migration_timing_on_real_db -- --ignored --nocapture
    #[test]
    #[ignore = "manual: needs REFINE_SA_TIMING_DB (a disposable copy)"]
    fn migration_timing_on_real_db() {
        let path = std::env::var("REFINE_SA_TIMING_DB")
            .expect("set REFINE_SA_TIMING_DB to a DISPOSABLE db copy");
        let conn = pragma::open_writer(std::path::Path::new(&path)).expect("open_writer");
        let ver: i64 = conn
            .pragma_query_value(None, "user_version", |r| r.get(0))
            .unwrap();
        if ver == SCHEMA_VERSION {
            println!("already v{SCHEMA_VERSION} — nothing to migrate");
            return;
        }
        let t = std::time::Instant::now();
        migrate(&conn).expect("migrate");
        println!("v{ver} -> v{SCHEMA_VERSION} migration: {:?}", t.elapsed());
        let n: i64 = conn
            .query_row("SELECT count(*) FROM part_search", [], |r| r.get(0))
            .unwrap();
        println!("part_search rows: {n}");
        let bad: i64 = conn
            .query_row(
                "SELECT count(*) FROM part_search ps WHERE (ps.rowid / 1048576) <> (SELECT time_created FROM msg WHERE id = ps.message_id)",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(bad, 0, "every rowid must encode its message's ms");
    }

    /// Quote-doubling inside the FTS phrase must survive (a no-op replace
    /// here turns every quote needle into an unterminated phrase → SQL
    /// error; the HTTP 200-only test passes vacuously under that bug, this
    /// one does not). Planted-cliff guard for the no_effect_replace class.
    #[test]
    fn quote_needle_never_errors() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("t.db");
        let conn = pragma::create_new(&p).unwrap();
        migrate(&conn).unwrap();
        seed(&conn);
        upsert(
            &conn,
            "p1",
            "m1",
            r#"plain text with embedded "quotes" inside"#,
        );
        for needle in ["quotes", r#"embedded "q"#, r#""#, r#"a" OR "b"#] {
            let res = search_parts(&p, needle, None, 10, 0);
            assert!(res.is_ok(), "needle {needle:?} must not error: {res:?}");
        }
        let (hits, _) = search_parts(&p, r#"embedded "q"#, None, 10, 0).unwrap();
        assert_eq!(hits.len(), 1, "doubled quote still finds the phrase");
    }

    /// EQP control (mutation: re-shape search to the pre-S-A
    /// time-join+sort form → "USE TEMP B-TREE" reappears → this reds).
    #[test]
    fn global_match_plan_is_an_fts_walk_without_sort() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("t.db");
        let conn = pragma::create_new(&p).unwrap();
        migrate(&conn).unwrap();
        seed(&conn);
        upsert(&conn, "p1", "m1", "needle one");
        upsert(&conn, "p2", "m2", "needle two");
        let mut stmt = conn
            .prepare(&format!("EXPLAIN QUERY PLAN {SEARCH_MATCH_GLOBAL}"))
            .unwrap();
        let plan: String = stmt
            .query_map(rusqlite::params!["\"needle\"", 10_i64, 0_i64], |r| {
                r.get::<_, String>(3)
            })
            .unwrap()
            .filter_map(|r| r.ok())
            .collect::<Vec<_>>()
            .join(" | ");
        assert!(plan.contains("part_search_fts"), "plan: {plan}");
        assert!(
            !plan.contains("TEMP B-TREE"),
            "sort must be gone (E1 winner shape); plan: {plan}"
        );
    }

    /// v9→v10 migration preserves the recorded contract order exactly —
    /// including the case insert-order and time-order disagree (the whole
    /// point of the encoding). Pre-computed with the OLD SQL shape, compared
    /// against search_parts AFTER the rebuild.
    #[test]
    fn v9_migration_preserves_contract_order() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("t.db");
        let conn = pragma::create_new(&p).unwrap();
        migrate(&conn).unwrap();
        // rewind to a v9-shaped part_search: auto rowids (insert order),
        // no session index, no rt trigger, no msg FK
        conn.execute_batch(
            "DROP TABLE part_search_fts;
             DROP TABLE part_search;
             CREATE TABLE part_search (
                 rowid      INTEGER PRIMARY KEY,
                 part_id    TEXT NOT NULL UNIQUE REFERENCES msg_part(id) ON DELETE CASCADE,
                 session_id TEXT NOT NULL,
                 message_id TEXT NOT NULL,
                 text       TEXT NOT NULL
             ) STRICT;
             CREATE VIRTUAL TABLE part_search_fts USING fts5(
                 text, content='part_search', content_rowid='rowid', tokenize='trigram'
             );
             CREATE TRIGGER part_search_ai AFTER INSERT ON part_search BEGIN
                 INSERT INTO part_search_fts(rowid, text) VALUES (new.rowid, new.text);
             END;
             CREATE TRIGGER part_search_ad AFTER DELETE ON part_search BEGIN
                 INSERT INTO part_search_fts(part_search_fts, rowid, text)
                 VALUES ('delete', old.rowid, old.text);
             END;
             CREATE TRIGGER part_search_au AFTER UPDATE ON part_search BEGIN
                 INSERT INTO part_search_fts(part_search_fts, rowid, text)
                 VALUES ('delete', old.rowid, old.text);
                 INSERT INTO part_search_fts(rowid, text) VALUES (new.rowid, new.text);
             END;",
        )
        .unwrap();
        seed(&conn); // parts p1(m1 t=100), p2(m2 t=500), p3(m3 t=200)
        upsert(&conn, "p1", "m1", "needle alpha");
        upsert(&conn, "p2", "m2", "needle beta");
        upsert(&conn, "p3", "m3", "needle gamma");
        // record expectation with the PRE-S-A contract SQL
        let expected: Vec<String> = conn
            .prepare(
                "SELECT ps.part_id FROM part_search ps JOIN msg m ON m.id = ps.message_id WHERE ps.rowid IN (SELECT rowid FROM part_search_fts WHERE part_search_fts MATCH '\"needle\"') ORDER BY m.time_created DESC, ps.rowid DESC",
            )
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .filter_map(|r| r.ok())
            .collect();
        assert_eq!(
            expected,
            vec!["p2", "p3", "p1"],
            "pre-migration contract order"
        );
        conn.pragma_update(None, "user_version", 9).unwrap();
        migrate(&conn).unwrap(); // runs arm9 → v10
        let ver: i64 = conn
            .pragma_query_value(None, "user_version", |r| r.get(0))
            .unwrap();
        assert_eq!(ver, SCHEMA_VERSION);
        let (hits, trunc) = search_parts(&p, "needle", None, 10, 0).unwrap();
        let got: Vec<&str> = hits.iter().map(|h| h.part_id.as_str()).collect();
        assert_eq!(got, expected, "post-migration search order == contract");
        assert!(!trunc);
        // LIKE path (<3 chars uses it too) — same order guarantee
        let (hits, _) = search_parts(&p, "ne", None, 10, 0).unwrap();
        let got: Vec<&str> = hits.iter().map(|h| h.part_id.as_str()).collect();
        assert_eq!(got, expected, "LIKE fallback order == contract");
        // scoped path stays scoped + ordered
        let (hits, _) = search_parts(&p, "needle", Some("ses_1"), 10, 0).unwrap();
        assert_eq!(hits.len(), 3);
    }
}
