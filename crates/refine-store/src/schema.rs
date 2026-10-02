//! Schema: STRICT tables, fixed-size metadata only, user_version gate.
//! Payloads never live here — they live in the blob store (STORAGE.md §4).

use anyhow::{Result, bail};
use rusqlite::Connection;

/// Bump when the schema changes; refuse to open mismatches with an actionable error
/// (reliary8/stria pattern: schema.rs user_version gate).
pub const SCHEMA_VERSION: i64 = 6;

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
    version_dirt  INTEGER NOT NULL DEFAULT 0
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

CREATE TABLE search_doc (
    id      INTEGER PRIMARY KEY,
    title   TEXT NOT NULL DEFAULT '',
    excerpt TEXT NOT NULL DEFAULT '',
    updated_at INTEGER NOT NULL DEFAULT 0
) STRICT;

CREATE VIRTUAL TABLE search_fts USING fts5(
    title, excerpt, content='search_doc', content_rowid='id', tokenize='unicode61'
);

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

    /// M0 decisive experiment (PLAN §13): FTS5 external-content LOCAL lifecycle
    /// must work on the bundled build; ATTACH-based designs are already falsified
    /// on-box (content-name qualification + trigger restrictions) — we assert the
    /// local design we chose actually functions.
    #[test]
    fn external_content_lifecycle_local() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("t.db");
        let conn = pragma::create_new(&p).unwrap();
        migrate(&conn).unwrap();
        conn.execute(
            "INSERT INTO search_doc (id, title, excerpt, updated_at) VALUES (1, 'hello world', 'some body', 1)",
            [],
        ).unwrap();
        conn.execute(
            "INSERT INTO search_fts(rowid, title, excerpt) SELECT id, title, excerpt FROM search_doc",
            [],
        ).unwrap();
        let n: i64 = conn
            .query_row(
                "SELECT count(*) FROM search_fts WHERE search_fts MATCH 'hello'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(n, 1);
        // update path: edit doc then rebuild its row
        conn.execute("UPDATE search_doc SET title='goodbye moon' WHERE id=1", [])
            .unwrap();
        conn.execute("DELETE FROM search_fts WHERE rowid=1", [])
            .unwrap();
        conn.execute(
            "INSERT INTO search_fts(rowid, title, excerpt) SELECT id, title, excerpt FROM search_doc WHERE id=1",
            [],
        ).unwrap();
        let n: i64 = conn
            .query_row(
                "SELECT count(*) FROM search_fts WHERE search_fts MATCH 'moon'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(n, 1);
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
