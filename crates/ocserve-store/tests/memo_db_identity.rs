//! Regression: the list/search memos are keyed by write-epoch, which is
//! PROCESS-GLOBAL — so two different databases in one process share an epoch
//! and can silently serve each other's bytes. Found 2026-10-07 when the
//! corpus parity gate ran two dbs in one test binary: the synthetic test
//! received the live db's session list (`list_wire_parity.rs` left/right
//! mismatch, and the kill-switch test failed in the opposite direction).
//!
//! Production has one db per process (serve), so the bug was latent — but it
//! made the gates unrunnable and is a real identity hole. Fix: memo hits
//! require epoch AND db path match; a path mismatch clears the slot.

use std::path::Path;

fn seed_session(db: &Path, id: &str, title: &str) {
    let conn = ocserve_store::pragma::create_new(db).unwrap();
    ocserve_store::schema::migrate(&conn).unwrap();
    conn.execute_batch(&format!(
        "INSERT INTO session (id, project_id, directory, path, slug, title, version,
                              cost, time_created, time_updated)
         VALUES ('{id}','p','/','p','s','{title}','1',0.0,1,1);"
    ))
    .unwrap();
}

#[test]
fn list_memo_never_serves_a_foreign_db() {
    let d1 = tempfile::tempdir().unwrap();
    let d2 = tempfile::tempdir().unwrap();
    let db1 = d1.path().join("a.db");
    let db2 = d2.path().join("b.db");
    seed_session(&db1, "ses_one", "first-db");
    seed_session(&db2, "ses_two", "second-db");

    // warm the memo from db1 (single slot, no writes after => epoch unchanged,
    // which is exactly the condition that used to leak)
    let a = ocserve_store::load_sessions_wire_bytes(&db1).unwrap();
    let a_str = String::from_utf8_lossy(&a).to_string();
    assert!(a_str.contains("ses_one"), "db1 gives db1: {a_str}");

    // db2 must NOT get db1's bytes
    let b = ocserve_store::load_sessions_wire_bytes(&db2).unwrap();
    let b_str = String::from_utf8_lossy(&b).to_string();
    assert!(
        b_str.contains("ses_two"),
        "memo leaked db1 bytes to db2: {b_str}"
    );
    assert!(
        !b_str.contains("ses_one"),
        "memo served db1's sessions to db2: {b_str}"
    );

    // and the Value path must agree with the bytes path on db2
    let dom = ocserve_store::load_sessions_wire(&db2).unwrap();
    let dom_bytes = serde_json::to_vec(&dom).unwrap();
    let got = ocserve_store::load_sessions_wire_bytes(&db2).unwrap();
    assert_eq!(&dom_bytes[..], &got[..], "db2 bytes must match db2 DOM");

    // db1 still serves db1 after being displaced
    let a2 = ocserve_store::load_sessions_wire_bytes(&db1).unwrap();
    let a2_str = String::from_utf8_lossy(&a2).to_string();
    assert!(a2_str.contains("ses_one"), "db1 displaced: {a2_str}");
}

#[test]
fn search_memo_never_serves_a_foreign_db() {
    // Identical SearchKey (same needle/scope/limit/offset) against two dbs
    // whose content DIFFERS — the exact condition that used to cross-serve.
    let d1 = tempfile::tempdir().unwrap();
    let d2 = tempfile::tempdir().unwrap();
    let db1 = d1.path().join("a.db");
    let db2 = d2.path().join("b.db");
    seed_session(&db1, "ses_one", "alpha-holder");
    seed_session(&db2, "ses_two", "alpha-holder");

    // db1 gets a searchable part; db2 has none.
    {
        let conn = ocserve_store::pragma::open_writer(&db1).unwrap();
        conn.execute_batch(
            "INSERT INTO msg (id, session_id, role, seq, time_created, info)
             VALUES ('m1','ses_one','user',1,100,'{}');
             INSERT INTO msg_part (id, message_id, session_id, seq, type, byte_len, inline, blob_sha)
             VALUES ('p1','m1','ses_one',1,'text',5,'{}',NULL);",
        )
        .unwrap();
        // v10 rowid contract: time_created * 1_048_576 + slot
        conn.execute(
            "INSERT INTO part_search (rowid, part_id, session_id, message_id, text)
             VALUES (104857600,'p1','ses_one','m1','alpha needle')",
            [],
        )
        .unwrap();
    }

    let (h1, _) = ocserve_store::search_parts(&db1, "alpha", None, 10, 0).unwrap();
    assert_eq!(h1.len(), 1, "db1 must find its own seeded row");

    // same needle against db2: if the memo were epoch-only this would return
    // db1's hit (epoch never bumped between the two calls)
    let (h2, _) = ocserve_store::search_parts(&db2, "alpha", None, 10, 0).unwrap();
    assert_eq!(h2.len(), 0, "search memo leaked db1's hit to db2: {h2:?}");

    // db1 still hits after displacement
    let (h3, _) = ocserve_store::search_parts(&db1, "alpha", None, 10, 0).unwrap();
    assert_eq!(h3.len(), 1, "db1 must be able to re-hit after a miss");
}
