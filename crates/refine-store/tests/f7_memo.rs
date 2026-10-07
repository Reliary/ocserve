//! PERF-10X F7/F8 — epoch-keyed memoization, integration level.
//!
//! Own PROCESS on purpose: the memo state, epoch and counters are process
//! globals — crate-level unit tests would race them against every other
//! store test that writes (Writer::spawn bumps the epoch). Everything here
//! also serializes behind `LOCK` so the env kill-switch flip (unsafe in
//! edition 2024 when any sibling thread might read env) has quiesced
//! siblings. See TEST-PROCESS isolation precedent: reader_accounting.rs.

use refine_store::{WriteOp, Writer, pragma, search_parts, writer};
use std::path::Path;
use std::sync::{Mutex, MutexGuard, OnceLock};

static LOCK: OnceLock<Mutex<()>> = OnceLock::new();

fn lock() -> MutexGuard<'static, ()> {
    LOCK.get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|e| e.into_inner())
}

fn seed(path: &Path) -> Writer {
    let w = Writer::spawn(path.to_path_buf()).unwrap();
    w.write(vec![
        WriteOp::Sql {
            sql: "INSERT INTO session (id, project_id, directory, path, slug, title, version, time_created, time_updated) VALUES ('ses_m','global','/w','s','s','t','1',1,1)".into(),
            params: vec![],
        },
        WriteOp::Sql {
            sql: "INSERT INTO msg (id, session_id, role, seq, time_created, info) VALUES ('m1','ses_m','user',1,100,'{}')".into(),
            params: vec![],
        },
        WriteOp::Sql {
            sql: "INSERT INTO msg_part (id, message_id, session_id, seq, type, byte_len, inline, blob_sha) VALUES ('p1','m1','ses_m',1,'text',4,'{}',NULL)".into(),
            params: vec![],
        },
        refine_store::part_search_upsert_ops("p1", "ses_m", "m1", "alpha unique token"),
    ])
    .unwrap();
    w
}

/// The contract in one test: same query twice = served from memo (a raw
/// no-epoch write stays invisible — proves the cache is engaged); a
/// WRITER-path write bumps the epoch and the next query sees it (proves
/// invalidation is exact, not TTL).
#[test]
fn memo_hits_until_writer_write_then_exact_invalidation() {
    let _g = lock();
    let dir = tempfile::tempdir().unwrap();
    let db = writer::db_path(dir.path());
    let w = seed(&db);

    let (hits, _) = search_parts(&db, "alpha", None, 10, 0).unwrap();
    assert_eq!(hits.len(), 1, "seeded row matches");

    // raw insert (NOT through a write funnel): no epoch bump happens, so a
    // correct memo must still serve the previous result. If this second
    // call returned 2, the cache is not engaged at all.
    {
        let conn = pragma::open_writer(&db).unwrap();
        conn.execute(
            "INSERT INTO msg (id, session_id, role, seq, time_created, info) VALUES ('m2','ses_m','user',2,200,'{}')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO msg_part (id, message_id, session_id, seq, type, byte_len, inline, blob_sha) VALUES ('p2','m2','ses_m',1,'text',4,'{}',NULL)",
            [],
        )
        .unwrap();
        // v10 rowid contract: 200ms * 1_048_576 + slot0
        conn.execute(
            "INSERT INTO part_search (rowid, part_id, session_id, message_id, text) VALUES (209715200,'p2','ses_m','m2','alpha second token')",
            [],
        )
        .unwrap();
    }
    let (hits2, _) = search_parts(&db, "alpha", None, 10, 0).unwrap();
    assert_eq!(
        hits2.len(),
        1,
        "raw no-epoch write must NOT appear — memo hit (cache engaged)"
    );

    // writer path: insert p3 via the writer (apply/run_loop => bump)
    w.write(vec![
        WriteOp::Sql {
            sql: "INSERT INTO msg (id, session_id, role, seq, time_created, info) VALUES ('m3','ses_m','user',3,300,'{}')".into(),
            params: vec![],
        },
        WriteOp::Sql {
            sql: "INSERT INTO msg_part (id, message_id, session_id, seq, type, byte_len, inline, blob_sha) VALUES ('p3','m3','ses_m',1,'text',4,'{}',NULL)".into(),
            params: vec![],
        },
        refine_store::part_search_upsert_ops("p3", "ses_m", "m3", "alpha third token"),
    ])
    .unwrap();
    let (hits3, _) = search_parts(&db, "alpha", None, 10, 0).unwrap();
    assert_eq!(
        hits3.len(),
        3,
        "writer write bumps epoch => exact invalidation (no staleness window)"
    );
}

/// Session list: memo serves until a writer write; direct writes invisible.
#[test]
fn list_memo_tracks_writes() {
    let _g = lock();
    let dir = tempfile::tempdir().unwrap();
    let db = writer::db_path(dir.path());
    let w = seed(&db);

    let l1 = refine_store::load_sessions_wire(&db).unwrap();
    assert_eq!(l1.len(), 1);

    // raw session insert — no bump — must stay invisible
    {
        let conn = pragma::open_writer(&db).unwrap();
        conn.execute(
            "INSERT INTO session (id, project_id, directory, path, slug, title, version, time_created, time_updated) VALUES ('ses_raw','global','/w','s','s','raw','1',9,9)",
            [],
        )
        .unwrap();
    }
    let l2 = refine_store::load_sessions_wire(&db).unwrap();
    assert_eq!(l2.len(), 1, "raw write invisible = memo engaged");

    w.write(vec![WriteOp::Sql {
        sql: "INSERT INTO session (id, project_id, directory, path, slug, title, version, time_created, time_updated) VALUES ('ses_w','global','/w','s','s','w','1',10,10)".into(),
        params: vec![],
    }])
    .unwrap();
    let l3 = refine_store::load_sessions_wire(&db).unwrap();
    // fresh read after the bump sees ALL rows: the writer one AND the raw
    // one that the memo had been (correctly) hiding — that's the proof the
    // slot was re-read, not patched.
    assert_eq!(l3.len(), 3, "writer write visible = exact invalidation");
}

/// Kill switches: env "0" disables both memos (behavioral: the raw write
/// from the hit test becomes visible immediately when search memo is off).
#[test]
fn env_kill_switches_disable_memos() {
    let _g = lock();
    // SAFETY: LOCK serializes every test in this process — no sibling
    // thread reads env during the flip (documented per edition-2024 rule).
    unsafe {
        std::env::set_var("REFINE_SEARCH_MEMO", "0");
        std::env::set_var("REFINE_LIST_MEMO", "0");
    }
    assert!(!refine_store::search_memo_enabled());
    assert!(!refine_store::list_memo_enabled());

    let dir = tempfile::tempdir().unwrap();
    let db = writer::db_path(dir.path());
    let w = seed(&db);
    let (h1, _) = search_parts(&db, "alpha", None, 10, 0).unwrap();
    assert_eq!(h1.len(), 1);
    {
        let conn = pragma::open_writer(&db).unwrap();
        conn.execute(
            "INSERT INTO msg (id, session_id, role, seq, time_created, info) VALUES ('m2','ses_m','user',2,200,'{}')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO msg_part (id, message_id, session_id, seq, type, byte_len, inline, blob_sha) VALUES ('p2','m2','ses_m',1,'text',4,'{}',NULL)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO part_search (rowid, part_id, session_id, message_id, text) VALUES (209715200,'p2','ses_m','m2','alpha second token')",
            [],
        )
        .unwrap();
    }
    let (h2, _) = search_parts(&db, "alpha", None, 10, 0).unwrap();
    assert_eq!(h2.len(), 2, "memo off => direct read sees raw write");
    // and a writer write with memo off still behaves (no cache to clear)
    w.write(vec![refine_store::part_search_upsert_ops(
        "p1",
        "ses_m",
        "m1",
        "alpha rewritten",
    )])
    .unwrap();
    let (h3, _) = search_parts(&db, "alpha", None, 10, 0).unwrap();
    assert_eq!(h3.len(), 2);
    unsafe {
        std::env::remove_var("REFINE_SEARCH_MEMO");
        std::env::remove_var("REFINE_LIST_MEMO");
    }
    assert!(refine_store::search_memo_enabled());
}
