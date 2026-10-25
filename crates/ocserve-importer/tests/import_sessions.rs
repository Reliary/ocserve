//! Explicit-session import gates: id selection, preflight guards, idempotent
//! replacement, todo adoption, and the live-target (incremental FTS) path.
//!
//! The synthetic source mimics the upstream schema columns the importer reads
//! (message/part/event verified against the live DB; todo lacks the id column
//! upstream, so the importer derives it).

use ocserve_importer::{IndexMode, import_last_n, import_sessions};
use rusqlite::{Connection, OpenFlags};

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

fn make_source(dir: &std::path::Path) -> std::path::PathBuf {
    let src = dir.join("source.db");
    let c = Connection::open(&src).unwrap();
    c.execute_batch(
        "CREATE TABLE session (id TEXT PRIMARY KEY, project_id TEXT NOT NULL DEFAULT 'global',
                                slug TEXT NOT NULL DEFAULT '', directory TEXT NOT NULL DEFAULT '',
                                title TEXT NOT NULL DEFAULT '', version TEXT NOT NULL DEFAULT '1',
                                path TEXT, agent TEXT, model TEXT,
                                cost REAL NOT NULL DEFAULT 0,
                                summary_additions INTEGER NOT NULL DEFAULT 0,
                                summary_deletions INTEGER NOT NULL DEFAULT 0,
                                summary_files INTEGER NOT NULL DEFAULT 0,
                                tokens_input INTEGER NOT NULL DEFAULT 0,
                                tokens_output INTEGER NOT NULL DEFAULT 0,
                                tokens_reasoning INTEGER NOT NULL DEFAULT 0,
                                tokens_cache_read INTEGER NOT NULL DEFAULT 0,
                                tokens_cache_write INTEGER NOT NULL DEFAULT 0,
                                time_created INTEGER NOT NULL, time_updated INTEGER NOT NULL);
         CREATE TABLE message (id TEXT PRIMARY KEY, session_id TEXT, time_created INTEGER,
                                time_updated INTEGER, data TEXT);
         CREATE TABLE part (id TEXT PRIMARY KEY, message_id TEXT, session_id TEXT,
                            time_created INTEGER, time_updated INTEGER, data TEXT);
         CREATE TABLE event (id INTEGER PRIMARY KEY, aggregate_id TEXT, seq INTEGER,
                             type TEXT, data TEXT);
         CREATE TABLE todo (session_id TEXT NOT NULL, content TEXT NOT NULL,
                            status TEXT NOT NULL, priority TEXT NOT NULL,
                            position INTEGER NOT NULL, time_created INTEGER NOT NULL,
                            time_updated INTEGER NOT NULL,
                            PRIMARY KEY (session_id, position));",
    )
    .unwrap();
    for (i, sid) in ["ses_want", "ses_other"].iter().enumerate() {
        // NOW-relative: imported events are pruned by the 30-day ring at the
        // end of the import, so a stale fixture timestamp would drop them.
        let t = now_ms() - 60_000 + i as i64;
        c.execute(
            "INSERT INTO session (id, title, time_created, time_updated) VALUES (?1, ?2, ?3, ?3)",
            rusqlite::params![sid, format!("session {sid}"), t],
        )
        .unwrap();
        let mid = format!("msg_{sid}");
        c.execute(
            "INSERT INTO message VALUES (?1, ?2, ?3, ?3, ?4)",
            rusqlite::params![
                mid,
                sid,
                t,
                format!("{{\"id\":\"{mid}\",\"sessionID\":\"{sid}\",\"role\":\"user\"}}")
            ],
        )
        .unwrap();
        c.execute(
            "INSERT INTO part VALUES (?1, ?2, ?3, ?4, ?4, ?5)",
            rusqlite::params![
                format!("prt_{sid}"),
                mid,
                sid,
                t,
                format!("{{\"id\":\"prt_{sid}\",\"type\":\"text\",\"text\":\"body of {sid}\"}}")
            ],
        )
        .unwrap();
        c.execute(
            "INSERT INTO event (aggregate_id, seq, type, data) VALUES (?1, 1, 'session.updated', ?2)",
            rusqlite::params![sid, format!("{{\"sid\":\"{sid}\"}}")],
        )
        .unwrap();
        c.execute(
            "INSERT INTO todo VALUES (?1, ?2, 'pending', 'high', 0, ?3, ?3)",
            rusqlite::params![sid, format!("task for {sid}"), t],
        )
        .unwrap();
    }
    drop(c);
    src
}

fn target_db(dir: &std::path::Path) -> std::path::PathBuf {
    ocserve_store::writer::db_path(dir)
}

fn counts(db: &std::path::Path) -> (i64, i64, i64, i64, i64) {
    let c = Connection::open_with_flags(db, OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
    (
        c.query_row("SELECT count(*) FROM session", [], |r| r.get(0))
            .unwrap(),
        c.query_row("SELECT count(*) FROM msg", [], |r| r.get(0))
            .unwrap(),
        c.query_row("SELECT count(*) FROM msg_part", [], |r| r.get(0))
            .unwrap(),
        c.query_row("SELECT count(*) FROM event", [], |r| r.get(0))
            .unwrap(),
        c.query_row("SELECT count(*) FROM todo", [], |r| r.get(0))
            .unwrap(),
    )
}

/// `session.path` is nullable upstream. Reading it as a String aborted the
/// whole import with `Invalid column type Null` on a real 2026-10-25 row.
#[test]
fn nullable_source_columns_do_not_abort_the_import() {
    let d = tempfile::tempdir().unwrap();
    let src = make_source(d.path());
    {
        let c = Connection::open(&src).unwrap();
        c.execute(
            "INSERT INTO session (id, title, path, time_created, time_updated) \
             VALUES ('ses_nullpath', 'no path', NULL, ?1, ?1)",
            [now_ms() - 60_000],
        )
        .unwrap();
    }
    let data = d.path().join("data");
    import_sessions(
        &src,
        &data,
        &["ses_nullpath".to_string()],
        false,
        IndexMode::Auto,
    )
    .unwrap();
    let c =
        Connection::open_with_flags(target_db(&data), OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
    let path: String = c
        .query_row(
            "SELECT path FROM session WHERE id='ses_nullpath'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(path, "", "a NULL source path becomes the empty default");
}

#[test]
fn explicit_ids_import_only_the_selected_session() {
    let d = tempfile::tempdir().unwrap();
    let src = make_source(d.path());
    let data = d.path().join("data");
    import_sessions(
        &src,
        &data,
        &["ses_want".to_string()],
        false,
        IndexMode::LiveSlow,
    )
    .unwrap();

    let db = target_db(&data);
    assert_eq!(counts(&db), (1, 1, 1, 1, 1), "only ses_want + payloads");
    let c = Connection::open_with_flags(&db, OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
    let id: String = c
        .query_row("SELECT id FROM session", [], |r| r.get(0))
        .unwrap();
    assert_eq!(id, "ses_want");
    let content: String = c
        .query_row("SELECT content FROM todo", [], |r| r.get(0))
        .unwrap();
    assert_eq!(content, "task for ses_want", "session task list adopted");
    // the derived id follows the runner's <session>_t<position> convention
    let tid: String = c
        .query_row("SELECT id FROM todo", [], |r| r.get(0))
        .unwrap();
    assert_eq!(tid, "ses_want_t0");

    // search projection covers the imported part (FTS trigger/index agree)
    let hits: i64 = c
        .query_row(
            "SELECT count(*) FROM part_search_fts WHERE part_search_fts MATCH '\"body of ses_want\"'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(hits, 1, "imported part is searchable");
    let docs: i64 = c
        .query_row("SELECT count(*) FROM part_search_fts_docsize", [], |r| {
            r.get(0)
        })
        .unwrap();
    let rows: i64 = c
        .query_row("SELECT count(*) FROM part_search", [], |r| r.get(0))
        .unwrap();
    assert_eq!(docs, rows, "docsize parity after import");
}

#[test]
fn unknown_and_duplicate_ids_are_refused_before_any_write() {
    let d = tempfile::tempdir().unwrap();
    let src = make_source(d.path());
    let data = d.path().join("data");

    let err = import_sessions(
        &src,
        &data,
        &["ses_ghost".to_string()],
        false,
        IndexMode::LiveSlow,
    )
    .unwrap_err();
    assert!(
        format!("{err}").contains("not found"),
        "unknown id must name the miss: {err}"
    );
    assert!(
        !target_db(&data).exists(),
        "preflight failure writes nothing"
    );

    let err = import_sessions(
        &src,
        &data,
        &["ses_want".to_string(), "ses_want".to_string()],
        false,
        IndexMode::LiveSlow,
    )
    .unwrap_err();
    assert!(format!("{err}").contains("duplicate"), "{err}");
    assert!(!target_db(&data).exists());
}

#[test]
fn existing_session_requires_force_and_force_replaces_exactly() {
    let d = tempfile::tempdir().unwrap();
    let src = make_source(d.path());
    let data = d.path().join("data");
    let ids = ["ses_want".to_string()];

    import_sessions(&src, &data, &ids, false, IndexMode::LiveSlow).unwrap();
    let db = target_db(&data);
    assert_eq!(counts(&db), (1, 1, 1, 1, 1));

    // re-running without --force is a named refusal, not a silent duplicate
    let err = import_sessions(&src, &data, &ids, false, IndexMode::LiveSlow).unwrap_err();
    assert!(format!("{err}").contains("already"), "{err}");
    assert_eq!(counts(&db), (1, 1, 1, 1, 1), "refusal must not mutate");

    // --force replaces: counts stay exactly one session's worth (no orphans)
    import_sessions(&src, &data, &ids, true, IndexMode::LiveSlow).unwrap();
    assert_eq!(
        counts(&db),
        (1, 1, 1, 1, 1),
        "replacement is idempotent (events/todos not duplicated)"
    );
}

#[test]
fn populated_target_keeps_the_incremental_fts_trigger() {
    let d = tempfile::tempdir().unwrap();
    let src = make_source(d.path());
    let data = d.path().join("data");

    // Start from a bulk (empty-target) import of one session...
    import_sessions(
        &src,
        &data,
        &["ses_other".to_string()],
        false,
        IndexMode::LiveSlow,
    )
    .unwrap();
    let db = target_db(&data);
    // ...then adopt the second into the POPULATED target: incremental path,
    // no DDL, trigger intact the whole time.
    import_sessions(
        &src,
        &data,
        &["ses_want".to_string()],
        false,
        IndexMode::LiveSlow,
    )
    .unwrap();

    let c = Connection::open_with_flags(&db, OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
    let trigger: i64 = c
        .query_row(
            "SELECT count(*) FROM sqlite_master WHERE type='trigger' AND name='part_search_ai'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(trigger, 1, "incremental import must never drop the trigger");
    let docs: i64 = c
        .query_row("SELECT count(*) FROM part_search_fts_docsize", [], |r| {
            r.get(0)
        })
        .unwrap();
    let rows: i64 = c
        .query_row("SELECT count(*) FROM part_search", [], |r| r.get(0))
        .unwrap();
    assert_eq!(
        docs, rows,
        "trigger indexed every incrementally inserted row"
    );
    assert_eq!(counts(&db), (2, 2, 2, 2, 2), "both sessions present");
}

#[test]
fn last_n_still_selects_by_recency() {
    let d = tempfile::tempdir().unwrap();
    let src = make_source(d.path());
    let data = d.path().join("data");
    import_last_n(&src, &data, 1).unwrap();

    let db = target_db(&data);
    let c = Connection::open_with_flags(&db, OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
    let id: String = c
        .query_row("SELECT id FROM session", [], |r| r.get(0))
        .unwrap();
    assert_eq!(id, "ses_other", "newest time_updated wins the LIMIT 1");
}

#[test]
fn large_live_import_requires_explicit_opt_in() {
    // The live path indexes every part through the trigram trigger. That is
    // how a 21k-part adoption ran for >15 minutes with no progress output,
    // so the preflight refuses it unless the operator says so explicitly.
    let d = tempfile::tempdir().unwrap();
    let src = make_source(d.path());
    let data = d.path().join("data");

    // Populate the target (bulk path) with one session.
    import_sessions(
        &src,
        &data,
        &["ses_other".to_string()],
        false,
        IndexMode::Auto,
    )
    .unwrap();

    // Make the second session large enough to trip the threshold.
    {
        let c = Connection::open(&src).unwrap();
        // One transaction: 20k autocommit inserts each fsync, which made this
        // setup take over a minute for no reason.
        c.execute_batch("BEGIN").unwrap();
        let t = now_ms() - 30_000;
        let stmt = "INSERT INTO part VALUES (?1, ?2, ?3, ?4, ?4, ?5)";
        for i in 0..ocserve_importer::LIVE_IMPORT_PARTS_WARN {
            let mid = "msg_ses_want";
            c.execute(
                stmt,
                rusqlite::params![
                    format!("prt_fill_{i}"),
                    mid,
                    "ses_want",
                    t,
                    format!("{{\"id\":\"prt_fill_{i}\",\"type\":\"text\",\"text\":\"f\"}}")
                ],
            )
            .unwrap();
        }
        c.execute_batch("COMMIT").unwrap();
    }

    let err = import_sessions(
        &src,
        &data,
        &["ses_want".to_string()],
        false,
        IndexMode::Auto,
    )
    .unwrap_err();
    let msg = format!("{err}");
    assert!(
        msg.contains("refusing a live import"),
        "expected the preflight refusal, got: {msg}"
    );
    assert!(
        msg.contains("--allow-slow-live"),
        "the refusal must name the way forward: {msg}"
    );
    // Nothing was written for the refused session.
    let c =
        Connection::open_with_flags(target_db(&data), OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
    let n: i64 = c
        .query_row(
            "SELECT count(*) FROM session WHERE id='ses_want'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(n, 0, "a refused import must not write the session row");
}

/// The fast path must be reachable on a POPULATED data dir. Before this test,
/// `bulk` was inferred from `existing == 0`, so stopping the server changed
/// nothing: an existing data dir still took the per-row trigger path.
#[test]
fn bulk_mode_reaches_the_fast_path_on_a_populated_target() {
    let d = tempfile::tempdir().unwrap();
    let src = make_source(d.path());
    let data = d.path().join("data");

    import_sessions(
        &src,
        &data,
        &["ses_other".to_string()],
        false,
        IndexMode::Auto,
    )
    .unwrap();
    let db = target_db(&data);
    let before = counts(&db);

    import_sessions(
        &src,
        &data,
        &["ses_want".to_string()],
        false,
        IndexMode::Bulk,
    )
    .unwrap();

    let c = Connection::open_with_flags(&db, OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
    // The rebuild covers BOTH sessions (the index is rebuilt from part_search).
    let docs: i64 = c
        .query_row("SELECT count(*) FROM part_search_fts_docsize", [], |r| {
            r.get(0)
        })
        .unwrap();
    let rows: i64 = c
        .query_row("SELECT count(*) FROM part_search", [], |r| r.get(0))
        .unwrap();
    assert_eq!(docs, rows, "bulk rebuild must index every row, old and new");
    let trigger: i64 = c
        .query_row(
            "SELECT count(*) FROM sqlite_master WHERE type='trigger' AND name='part_search_ai'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        trigger, 1,
        "the trigger must be recreated after the rebuild"
    );
    let after = counts(&db);
    assert_eq!(after.0, before.0 + 1, "both sessions are present");
    assert!(after.3 > before.3, "the new events landed");
}

/// `--allow-slow-live` still bypasses the large-session refusal.
#[test]
fn live_slow_mode_bypasses_the_preflight_and_keeps_the_trigger() {
    let d = tempfile::tempdir().unwrap();
    let src = make_source(d.path());
    let data = d.path().join("data");

    import_sessions(
        &src,
        &data,
        &["ses_other".to_string()],
        false,
        IndexMode::Auto,
    )
    .unwrap();
    {
        let c = Connection::open(&src).unwrap();
        c.execute_batch("BEGIN").unwrap();
        let t = now_ms() - 30_000;
        for i in 0..ocserve_importer::LIVE_IMPORT_PARTS_WARN {
            c.execute(
                "INSERT INTO part VALUES (?1, ?2, ?3, ?4, ?4, ?5)",
                rusqlite::params![
                    format!("prt_fill_{i}"),
                    "msg_ses_want",
                    "ses_want",
                    t,
                    format!("{{\"id\":\"prt_fill_{i}\",\"type\":\"text\",\"text\":\"f\"}}")
                ],
            )
            .unwrap();
        }
        c.execute_batch("COMMIT").unwrap();
    }

    import_sessions(
        &src,
        &data,
        &["ses_want".to_string()],
        false,
        IndexMode::LiveSlow,
    )
    .unwrap();
    let c =
        Connection::open_with_flags(target_db(&data), OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
    let docsize: i64 = c
        .query_row("SELECT count(*) FROM part_search_fts_docsize", [], |r| {
            r.get(0)
        })
        .unwrap();
    let rows: i64 = c
        .query_row("SELECT count(*) FROM part_search", [], |r| r.get(0))
        .unwrap();
    assert_eq!(
        docsize, rows,
        "the live path indexed every row via the trigger"
    );
}
