//! The import CLI surface, exercised through the real binary.
//!
//! The importer library gained `--session` / `--force` before the CLI did, so
//! the documented command was unreachable and nobody noticed: every test
//! called the library directly. These tests run the shipped binary and assert
//! the flags exist, do what the README says, and refuse the destructive cases
//! without writing anything.

use rusqlite::Connection;
use std::process::Command;

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Minimal source database with the columns the importer reads.
fn make_source(dir: &std::path::Path) -> std::path::PathBuf {
    let src = dir.join("source.db");
    let c = Connection::open(&src).unwrap();
    c.execute_batch(
        "CREATE TABLE session (id TEXT PRIMARY KEY, project_id TEXT NOT NULL DEFAULT 'global',
                                slug TEXT NOT NULL DEFAULT '', directory TEXT NOT NULL DEFAULT '',
                                title TEXT NOT NULL DEFAULT '', version TEXT NOT NULL DEFAULT '1',
                                path TEXT NOT NULL DEFAULT '', agent TEXT, model TEXT,
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
                             type TEXT, data TEXT);",
    )
    .unwrap();
    let t = now_ms() - 60_000;
    for (i, sid) in ["ses_one", "ses_two"].iter().enumerate() {
        c.execute(
            "INSERT INTO session (id, title, time_created, time_updated) VALUES (?1, ?2, ?3, ?3)",
            rusqlite::params![sid, format!("session {sid}"), t + i as i64],
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
    }
    src
}

fn run(args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_ocserve"))
        .args(args)
        .output()
        .expect("run ocserve")
}

fn session_count(data_dir: &std::path::Path) -> i64 {
    let db = data_dir.join("ocserve.db");
    if !db.exists() {
        return -1;
    }
    let c = Connection::open_with_flags(db, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
    c.query_row("SELECT count(*) FROM session", [], |r| r.get(0))
        .unwrap()
}

#[test]
fn named_session_is_imported_and_force_is_required_to_replace_it() {
    let d = tempfile::tempdir().unwrap();
    let src = make_source(d.path());
    let data = d.path().join("data");
    let src = src.to_str().unwrap();
    let data = data.to_str().unwrap();

    let first = run(&[
        "import",
        "--source",
        src,
        "--session",
        "ses_one",
        "--data-dir",
        data,
    ]);
    assert!(
        first.status.success(),
        "named import failed: {}",
        String::from_utf8_lossy(&first.stderr)
    );
    assert_eq!(session_count(std::path::Path::new(data)), 1);

    // Without --force: a refusal, and the target is untouched.
    let second = run(&[
        "import",
        "--source",
        src,
        "--session",
        "ses_one",
        "--data-dir",
        data,
    ]);
    assert!(!second.status.success(), "re-import must be refused");
    let err = String::from_utf8_lossy(&second.stderr);
    assert!(err.contains("--force"), "the refusal names the flag: {err}");
    assert_eq!(session_count(std::path::Path::new(data)), 1);

    // With --force: replaced in place, still one session.
    let third = run(&[
        "import",
        "--source",
        src,
        "--session",
        "ses_one",
        "--force",
        "--data-dir",
        data,
    ]);
    assert!(
        third.status.success(),
        "force import failed: {}",
        String::from_utf8_lossy(&third.stderr)
    );
    assert_eq!(session_count(std::path::Path::new(data)), 1);
}

#[test]
fn bulk_and_allow_slow_live_are_mutually_exclusive() {
    let d = tempfile::tempdir().unwrap();
    let src = make_source(d.path());
    let data = d.path().join("data");
    let out = run(&[
        "import",
        "--source",
        src.to_str().unwrap(),
        "--session",
        "ses_one",
        "--bulk",
        "--allow-slow-live",
        "--data-dir",
        data.to_str().unwrap(),
    ]);
    assert!(
        !out.status.success(),
        "conflicting index modes must be refused"
    );
    assert_eq!(session_count(&data), -1, "nothing may be written");
}

#[test]
fn bulk_on_a_populated_target_keeps_the_index_complete() {
    let d = tempfile::tempdir().unwrap();
    let src = make_source(d.path());
    let data = d.path().join("data");
    let src = src.to_str().unwrap();
    let data = data.to_str().unwrap();

    let seed = run(&[
        "import",
        "--source",
        src,
        "--session",
        "ses_one",
        "--data-dir",
        data,
    ]);
    assert!(
        seed.status.success(),
        "{}",
        String::from_utf8_lossy(&seed.stderr)
    );

    let bulk = run(&[
        "import",
        "--source",
        src,
        "--session",
        "ses_two",
        "--bulk",
        "--data-dir",
        data,
    ]);
    assert!(
        bulk.status.success(),
        "bulk import failed: {}",
        String::from_utf8_lossy(&bulk.stderr)
    );
    assert_eq!(session_count(std::path::Path::new(data)), 2);

    let c = Connection::open_with_flags(
        std::path::Path::new(data).join("ocserve.db"),
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )
    .unwrap();
    let docs: i64 = c
        .query_row("SELECT count(*) FROM part_search_fts_docsize", [], |r| {
            r.get(0)
        })
        .unwrap();
    let rows: i64 = c
        .query_row("SELECT count(*) FROM part_search", [], |r| r.get(0))
        .unwrap();
    let trigger: i64 = c
        .query_row(
            "SELECT count(*) FROM sqlite_master WHERE type='trigger' AND name='part_search_ai'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(docs, rows, "bulk rebuild indexes both sessions");
    assert_eq!(trigger, 1, "the trigger is restored after the rebuild");
}

#[test]
fn force_without_a_named_session_is_rejected_before_any_write() {
    let d = tempfile::tempdir().unwrap();
    let src = make_source(d.path());
    let data = d.path().join("data");
    let out = run(&[
        "import",
        "--source",
        src.to_str().unwrap(),
        "--force",
        "--data-dir",
        data.to_str().unwrap(),
    ]);
    assert!(!out.status.success());
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(
        err.contains("--force requires at least one --session"),
        "unexpected error: {err}"
    );
    assert_eq!(
        session_count(&data),
        -1,
        "a rejected flag combination must not create a database"
    );
}

#[test]
fn help_documents_the_session_flags() {
    let out = run(&["import", "--help"]);
    let text = String::from_utf8_lossy(&out.stdout);
    for needle in [
        "--session",
        "--force",
        "--allow-slow-live",
        "--bulk",
        "--limit",
    ] {
        assert!(
            text.contains(needle),
            "import --help lacks {needle}:\n{text}"
        );
    }
}
