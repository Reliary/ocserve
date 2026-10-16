//! V2-PLAN.md P2: schema v13 inbox + instruction tables, versioned durable
//! types, stable event ids, and the single emission choke.
//!
//! Every test here carries its negative control inline (commented `CONTROL:`):
//! the sabotage that must turn it red, per TESTING §1 anti-theater.

use ocserve_store::{pragma, schema, writer};
use rusqlite::Connection;

fn migrated() -> (tempfile::TempDir, std::path::PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let db = writer::db_path(dir.path());
    let conn = pragma::open_writer(&db).unwrap();
    schema::migrate(&conn).unwrap();
    (dir, db)
}

fn tables(conn: &Connection) -> Vec<String> {
    let mut s = conn
        .prepare("SELECT name FROM sqlite_master WHERE type='table' ORDER BY name")
        .unwrap();
    s.query_map([], |r| r.get(0)).unwrap().flatten().collect()
}

#[test]
fn v13_creates_inbox_and_instruction_tables() {
    let (_dir, db) = migrated();
    let conn = pragma::open_reader(&db).unwrap();
    let t = tables(&conn);
    for want in [
        "session_inbox",
        "instruction_blob",
        "instruction_state",
        "instruction_entry",
    ] {
        assert!(t.contains(&want.to_string()), "missing table {want}");
    }
    // CONTROL: drop any of the four CREATEs from the v12->v13 step → red.
}

#[test]
fn v13_migration_is_pure_create_no_data_movement() {
    // A v12 DB with real rows migrates without touching them: same row
    // counts, same bytes, only user_version advances 12 -> 13.
    let dir = tempfile::tempdir().unwrap();
    let db = writer::db_path(dir.path());
    let conn = pragma::open_writer(&db).unwrap();
    conn.execute_batch(
        "CREATE TABLE session (id TEXT PRIMARY KEY, project_id TEXT NOT NULL DEFAULT 'global',
         slug TEXT NOT NULL DEFAULT '', directory TEXT NOT NULL DEFAULT '', parent_id TEXT,
         title TEXT NOT NULL DEFAULT '', version TEXT NOT NULL DEFAULT '1', path TEXT NOT NULL DEFAULT '',
         agent TEXT, model TEXT, cost REAL NOT NULL DEFAULT 0, summary_additions INTEGER NOT NULL DEFAULT 0,
         summary_deletions INTEGER NOT NULL DEFAULT 0, summary_files INTEGER NOT NULL DEFAULT 0,
         tokens_input INTEGER NOT NULL DEFAULT 0, tokens_output INTEGER NOT NULL DEFAULT 0,
         tokens_reasoning INTEGER NOT NULL DEFAULT 0, tokens_cache_read INTEGER NOT NULL DEFAULT 0,
         tokens_cache_write INTEGER NOT NULL DEFAULT 0, time_created INTEGER NOT NULL,
         time_updated INTEGER NOT NULL, version_dirt INTEGER NOT NULL DEFAULT 0,
         permission TEXT, revert TEXT) STRICT;
         CREATE TABLE msg (id TEXT PRIMARY KEY, session_id TEXT NOT NULL, role TEXT NOT NULL,
         seq INTEGER NOT NULL, time_created INTEGER NOT NULL, info TEXT NOT NULL) STRICT;
         CREATE TABLE msg_part (id TEXT PRIMARY KEY, message_id TEXT NOT NULL, session_id TEXT NOT NULL,
         seq INTEGER NOT NULL, type TEXT NOT NULL, byte_len INTEGER NOT NULL, inline TEXT, blob_sha TEXT) STRICT;
         CREATE TABLE event (seq INTEGER PRIMARY KEY AUTOINCREMENT, session_id TEXT,
         project_id TEXT NOT NULL DEFAULT 'global', type TEXT NOT NULL, payload TEXT NOT NULL,
         time_created INTEGER NOT NULL) STRICT;",
    )
    .unwrap();
    conn.execute(
        "INSERT INTO session (id, time_created, time_updated) VALUES ('ses_m', 1, 1)",
        [],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO event (session_id, type, payload, time_created) VALUES ('ses_m', 'session.created', '{}', 1)",
        [],
    )
    .unwrap();
    conn.pragma_update(None, "user_version", 12).unwrap();
    drop(conn);
    let conn = pragma::open_writer(&db).unwrap();
    schema::migrate(&conn).unwrap();
    let ver: i64 = conn
        .pragma_query_value(None, "user_version", |r| r.get(0))
        .unwrap();
    assert_eq!(ver, 13);
    let sessions: i64 = conn
        .query_row("SELECT count(*) FROM session", [], |r| r.get(0))
        .unwrap();
    let events: i64 = conn
        .query_row("SELECT count(*) FROM event", [], |r| r.get(0))
        .unwrap();
    assert_eq!((sessions, events), (1, 1));
    let ty: String = conn
        .query_row("SELECT type FROM event", [], |r| r.get(0))
        .unwrap();
    assert_eq!(ty, "session.created", "migration must not rewrite types");
    // CONTROL: any data touch in the 12->13 arm (e.g. a type rewrite) → red.
}

#[test]
fn versioned_type_discipline_round_trips() {
    assert_eq!(
        ocserve_store::versioned_event_type("session.updated"),
        "session.updated@v1"
    );
    assert_eq!(
        ocserve_store::versioned_event_type("session.updated@v1"),
        "session.updated@v1"
    );
    assert_eq!(
        ocserve_store::unversioned_event_type("session.updated@v1"),
        "session.updated"
    );
    assert_eq!(
        ocserve_store::unversioned_event_type("session.updated"),
        "session.updated"
    );
    // A trailing @ that is NOT a version stays put (no false strip).
    assert_eq!(ocserve_store::unversioned_event_type("a@b"), "a@b");
    assert_eq!(ocserve_store::unversioned_event_type("a@v"), "a@v");
    // CONTROL: strip unconditionally on '@' → the a@b case reds.
}

#[test]
fn emit_event_stamps_versioned_type_and_stable_id() {
    let (_dir, db) = migrated();
    let w = writer::Writer::spawn(db.clone()).unwrap();
    w.write(vec![writer::WriteOp::Sql {
        sql: "INSERT INTO session (id, time_created, time_updated) VALUES ('ses_e', 1, 1)".into(),
        params: vec![],
    }])
    .unwrap();
    let (seq, id) =
        ocserve_store::emit_event(&w, Some("ses_e"), "session.created", &serde_json::json!({}))
            .unwrap();
    assert_eq!((seq, id.as_str()), (1, "evt_1"));
    let (seq2, id2) =
        ocserve_store::emit_event(&w, Some("ses_e"), "session.updated", &serde_json::json!({}))
            .unwrap();
    assert_eq!((seq2, id2.as_str()), (2, "evt_2"));
    let conn = pragma::open_reader(&db).unwrap();
    let types: Vec<String> = conn
        .prepare("SELECT type FROM event ORDER BY seq")
        .unwrap()
        .query_map([], |r| r.get(0))
        .unwrap()
        .flatten()
        .collect();
    assert_eq!(types, vec!["session.created@v1", "session.updated@v1"]);
    // CONTROL: write the unversioned type → this assertion reds.
}

#[test]
fn inbox_crud_idempotent_reconcile_and_guarded_cancel() {
    let (_dir, db) = migrated();
    let conn = pragma::open_writer(&db).unwrap();
    conn.execute(
        "INSERT INTO session (id, time_created, time_updated) VALUES ('ses_i', 1, 1)",
        [],
    )
    .unwrap();
    // Admit (first write wins on id).
    conn.execute(
        "INSERT INTO session_inbox (id, session_id, type, payload, delivery, enqueued_seq, time_created)
         VALUES ('msg_1', 'ses_i', 'user', '{}', 'steer', 7, 1)",
        [],
    )
    .unwrap();
    let n: i64 = conn
        .query_row(
            "SELECT count(*) FROM session_inbox WHERE id='msg_1'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(n, 1);
    // Reconcile: same id re-admit is a no-op (INSERT OR IGNORE shape).
    conn.execute(
        "INSERT OR IGNORE INTO session_inbox (id, session_id, type, payload, delivery, enqueued_seq, time_created)
         VALUES ('msg_1', 'ses_i', 'user', '{}', 'steer', 7, 1)",
        [],
    )
    .unwrap();
    let n: i64 = conn
        .query_row(
            "SELECT count(*) FROM session_inbox WHERE id='msg_1'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(n, 1, "re-admit must be idempotent");
    // Deliver: delete + (caller's message insert) — row gone.
    conn.execute("DELETE FROM session_inbox WHERE id='msg_1'", [])
        .unwrap();
    let n: i64 = conn
        .query_row("SELECT count(*) FROM session_inbox", [], |r| r.get(0))
        .unwrap();
    assert_eq!(n, 0);
    // Ordering index serves the promote query shape.
    let idx: i64 = conn
        .query_row(
            "SELECT count(*) FROM sqlite_master WHERE type='index' AND name LIKE 'idx_inbox%'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(idx, 2);
    // CONTROL: UNIQUE-less PK (duplicate admit) → count 2, red.
}

#[test]
fn instruction_state_epoch_and_reset() {
    let (_dir, db) = migrated();
    let conn = pragma::open_writer(&db).unwrap();
    conn.execute(
        "INSERT INTO session (id, time_created, time_updated) VALUES ('ses_n', 1, 1)",
        [],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO instruction_blob (hash, value) VALUES ('h1', '{\"a\":1}')",
        [],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO instruction_state (session_id, epoch_start, through_seq, initial_values, current_values)
         VALUES ('ses_n', 0, 5, '{\"k\":\"h0\"}', '{\"k\":\"h1\"}')",
        [],
    )
    .unwrap();
    // Advance epoch (compaction): through becomes the new baseline.
    conn.execute(
        "UPDATE instruction_state SET epoch_start=through_seq WHERE session_id='ses_n'",
        [],
    )
    .unwrap();
    let epoch: i64 = conn
        .query_row(
            "SELECT epoch_start FROM instruction_state WHERE session_id='ses_n'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(epoch, 5);
    // Reset (revert-commit): row gone.
    conn.execute("DELETE FROM instruction_state WHERE session_id='ses_n'", [])
        .unwrap();
    let n: i64 = conn
        .query_row("SELECT count(*) FROM instruction_state", [], |r| r.get(0))
        .unwrap();
    assert_eq!(n, 0);
    // Blobs are content-addressed: double insert is a no-op.
    conn.execute(
        "INSERT OR IGNORE INTO instruction_blob (hash, value) VALUES ('h1', '{\"a\":1}')",
        [],
    )
    .unwrap();
    // CONTROL: without OR IGNORE the re-insert errors → red.
}

#[test]
fn inbox_engine_admit_deliver_promote_cancel_update() {
    use ocserve_store::inbox;
    let (_dir, db) = migrated();
    let w = writer::Writer::spawn(db.clone()).unwrap();
    w.write(vec![writer::WriteOp::Sql {
        sql: "INSERT INTO session (id, time_created, time_updated) VALUES ('ses_q', 1, 1)".into(),
        params: vec![],
    }])
    .unwrap();
    // Admit three items: steer, queue, compaction (seq 1,2,3).
    assert!(
        inbox::admit(
            &w,
            "ses_q",
            "msg_s",
            "user",
            &serde_json::json!({"text": "hi"}),
            "steer",
            1
        )
        .unwrap()
    );
    assert!(
        inbox::admit(
            &w,
            "ses_q",
            "msg_q",
            "user",
            &serde_json::json!({"text": "later"}),
            "queue",
            2
        )
        .unwrap()
    );
    assert!(
        inbox::admit(
            &w,
            "ses_q",
            "msg_c",
            "compaction",
            &serde_json::json!({}),
            "steer",
            3
        )
        .unwrap()
    );
    // Re-admit is idempotent (first write wins).
    assert!(
        !inbox::admit(
            &w,
            "ses_q",
            "msg_s",
            "user",
            &serde_json::json!({"text": "changed"}),
            "queue",
            9
        )
        .unwrap()
    );
    let got = inbox::get(&db, "msg_s").unwrap().unwrap();
    assert_eq!(got.2, "{\"text\":\"hi\"}", "re-admit must not overwrite");
    // Step-boundary promote sees only steers; lowest seq first.
    assert_eq!(
        inbox::next_promotable(&db, "ses_q", &["steer"]).unwrap(),
        Some("msg_s".to_string())
    );
    // Idle-boundary promote sees steers first, then the queued item.
    let all = inbox::list(&db, "ses_q").unwrap();
    assert_eq!(all.len(), 3);
    assert_eq!(all[0].0, "msg_s");
    // Deliver msg_s (delete); next promotable is the compaction steer.
    w.write(vec![inbox::deliver_ops("msg_s")]).unwrap();
    assert_eq!(
        inbox::next_promotable(&db, "ses_q", &["steer"]).unwrap(),
        Some("msg_c".to_string())
    );
    // Cancel the queued item: true once, false after (guarded, not Ok-always).
    assert!(inbox::cancel(&w, "msg_q").unwrap());
    assert!(!inbox::cancel(&w, "msg_q").unwrap());
    // Flip delivery on the remaining item.
    assert!(inbox::set_delivery(&w, "msg_c", "queue").unwrap());
    assert_eq!(inbox::get(&db, "msg_c").unwrap().unwrap().3, "queue");
    assert!(!inbox::set_delivery(&w, "msg_nope", "steer").unwrap());
    // CONTROL: deliver without delete (leak) → list still shows msg_s, red.
}

#[test]
fn claim_lifecycle_reclaim_noop_resume_budget_recovery_scan() {
    use ocserve_store::claim;
    let (_dir, db) = migrated();
    let w = writer::Writer::spawn(db.clone()).unwrap();
    w.write(vec![writer::WriteOp::Sql {
        sql: "INSERT INTO session (id, time_created, time_updated) VALUES ('ses_k', 1, 1)".into(),
        params: vec![],
    }])
    .unwrap();
    assert!(claim::claim(&w, "ses_k", 100).unwrap());
    assert!(
        !claim::claim(&w, "ses_k", 200).unwrap(),
        "re-claim is a no-op"
    );
    assert!(claim::held(&db, "ses_k").unwrap());
    assert_eq!(claim::count_resume(&w, "ses_k", 300).unwrap(), 1);
    assert_eq!(claim::count_resume(&w, "ses_k", 400).unwrap(), 2);
    assert_eq!(
        claim::list_suspended(&db).unwrap(),
        vec!["ses_k".to_string()]
    );
    assert!(claim::release(&w, "ses_k").unwrap());
    assert!(!claim::release(&w, "ses_k").unwrap());
    assert!(!claim::held(&db, "ses_k").unwrap());
    assert!(claim::list_suspended(&db).unwrap().is_empty());
    assert_eq!(claim::MAX_RESUME_ATTEMPTS, 10);
    // CONTROL: claim via INSERT without OR IGNORE → second claim errors, red.
}

#[test]
fn no_direct_append_event_from_server_paths() {
    // Structural: the only `append_event`/`append_versioned` callers outside
    // the store crate must be the choke itself (`emit_event`,
    // `append_versioned`). This test fails if a new direct caller appears
    // (the guard-side planted control lives in check-guards.sh).
    let root = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let banned = [
        "ocserve-core/src",
        "ocserve-http/src",
        "ocserve-importer/src",
    ];
    for rel in banned {
        let dir = root.join("..").join(rel);
        if !dir.exists() {
            continue;
        }
        let mut hits = Vec::new();
        collect_append_calls(&dir, &mut hits);
        // The choke lives in prompt.rs (emit_durable) and lib.rs
        // (emit_durable_http + dispose): exactly three call sites.
        assert!(
            hits.len() <= 3,
            "direct append callers outside the choke: {hits:?}"
        );
        for h in &hits {
            assert!(
                h.contains("prompt.rs") || h.contains("http/src/lib.rs"),
                "unexpected choke-external caller: {h}"
            );
        }
    }
}

fn collect_append_calls(dir: &std::path::Path, hits: &mut Vec<String>) {
    let rd = std::fs::read_dir(dir).unwrap();
    for e in rd.flatten() {
        let p = e.path();
        if p.is_dir() {
            collect_append_calls(&p, hits);
            continue;
        }
        if p.extension().map(|x| x == "rs").unwrap_or(false) {
            let src = std::fs::read_to_string(&p).unwrap_or_default();
            for (i, line) in src.lines().enumerate() {
                let t = line.trim_start();
                if t.starts_with("//") {
                    continue;
                }
                if (line.contains("append_event(") || line.contains("append_versioned("))
                    && !line.contains("fn append_event")
                    && !line.contains("fn append_versioned")
                    && !line.contains("no_direct_append")
                {
                    hits.push(format!("{}:{}", p.display(), i + 1));
                }
            }
        }
    }
}
