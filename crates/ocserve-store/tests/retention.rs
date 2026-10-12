//! STORAGE §4 ring bound + §5 backup drill (TESTING §4: boundary + state
//! tests with negative controls: remove the bound → test must fail).

use ocserve_store::{backup_to, pragma, prune_events, schema};
use rusqlite::Connection;

fn db_with_events(n: i64, now: i64) -> Connection {
    let conn = Connection::open_in_memory().unwrap();
    schema::migrate(&conn).unwrap();
    conn.execute(
        "INSERT INTO session (id, project_id, directory, path, slug, title, version,
                              time_created, time_updated)
         VALUES ('ses_a', 'global', '/w', 's', 's', 't', '1', 1, 1)",
        [],
    )
    .unwrap();
    conn.execute(
        "INSERT INTO session (id, project_id, directory, path, slug, title, version,
                              time_created, time_updated)
         VALUES ('ses_b', 'global', '/w', 's', 's', 't', '1', 1, 1)",
        [],
    )
    .unwrap();
    for i in 0..n {
        conn.execute(
            "INSERT INTO event (session_id, project_id, type, payload, time_created)
             VALUES ('ses_a', 'global', 'x', '{}', ?1)",
            [now - i], // seq = rowid order: newest first inserted? seq AUTOINCREMENT ascending per insert order
        )
        .unwrap();
        conn.execute(
            "INSERT INTO event (session_id, project_id, type, payload, time_created)
             VALUES ('ses_b', 'global', 'x', '{}', ?1)",
            [now],
        )
        .unwrap();
    }
    conn
}

#[test]
fn ring_cap_keeps_newest_per_session_and_is_independent() {
    let now = 1_800_000_000_000i64;
    let conn = db_with_events(120, now); // ses_a: 120 rows (mixed ages), ses_b: 120 fresh
    let pruned = prune_events(&conn, now, 100, 30 * 24 * 3_600_000).unwrap();
    let a: i64 = conn
        .query_row(
            "SELECT count(*) FROM event WHERE session_id='ses_a'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    let b: i64 = conn
        .query_row(
            "SELECT count(*) FROM event WHERE session_id='ses_b'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        a, 100,
        "ses_a capped at 100 (negative control: no cap → 120)"
    );
    assert_eq!(b, 100, "ses_b capped independently");
    assert_eq!(pruned, 40, "20 pruned per session");
    // survivors of ses_a are the NEWEST by seq: the age filter dropped old
    // rows first, cap keeps the rest — oldest remaining seq must be the
    // (120-20)=100th newest.
    let min_created: i64 = conn
        .query_row(
            "SELECT MIN(time_created) FROM event WHERE session_id='ses_a'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert!(
        min_created > now - 30 * 24 * 3_600_000,
        "age window respected"
    );
}

#[test]
fn age_window_drops_old_but_never_unknown_timestamps() {
    let now = 1_800_000_000_000i64;
    let conn = db_with_events(5, now);
    // one row with unknown timestamp (0) — must survive age pruning
    conn.execute(
        "INSERT INTO event (session_id, project_id, type, payload, time_created)
         VALUES ('ses_a', 'global', 'x', '{}', 0)",
        [],
    )
    .unwrap();
    // make all ses_b rows ancient
    conn.execute(
        "UPDATE event SET time_created = 1 WHERE session_id='ses_b'",
        [],
    )
    .unwrap();
    prune_events(&conn, now, 10_000, 30 * 24 * 3_600_000).unwrap();
    let zero: i64 = conn
        .query_row(
            "SELECT count(*) FROM event WHERE time_created = 0",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(zero, 1, "unknown-timestamp rows never age-pruned");
    let b: i64 = conn
        .query_row(
            "SELECT count(*) FROM event WHERE session_id='ses_b'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        b, 0,
        "ancient rows dropped (negative control: no prune → 120)"
    );
}

#[test]
fn vacuum_into_backup_restores_clean() {
    let dir = tempfile::tempdir().unwrap();
    let dbp = dir.path().join("ocserve.db");
    {
        let conn = Connection::open(&dbp).unwrap();
        schema::migrate(&conn).unwrap();
        conn.execute(
            "INSERT INTO session (id, project_id, directory, path, slug, title, version,
                                  time_created, time_updated)
             VALUES ('ses_x', 'global', '/w', 's', 's', 't', '1', 1, 1)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO msg (id, session_id, role, seq, time_created, info)
             VALUES ('m1', 'ses_x', 'user', 1, 1, '{}')",
            [],
        )
        .unwrap();
        let dest = dir.path().join("backup.db");
        backup_to(&conn, &dest).unwrap();
        // restore drill: copy back, integrity + counts
        let restored = dir.path().join("restored.db");
        std::fs::copy(&dest, &restored).unwrap();
        let r = pragma::open_reader(&restored).unwrap();
        let ic: String = r
            .query_row("PRAGMA integrity_check", [], |row| row.get(0))
            .unwrap();
        assert_eq!(ic, "ok", "restored DB passes integrity_check");
        let n: i64 = r
            .query_row("SELECT count(*) FROM msg", [], |row| row.get(0))
            .unwrap();
        assert_eq!(n, 1, "data survived backup/restore");
        // second backup to same dest must fail (refuse overwrite)
        assert!(backup_to(&conn, &dest).is_err(), "overwrite refused");
    }
}

// ---- response-time column merge (oc-remote parse contract) ----

#[test]
fn load_messages_merges_column_ids_into_imported_shaped_rows() {
    use ocserve_store::{Writer, load_messages};
    let dir = tempfile::tempdir().unwrap();
    let db = ocserve_store::writer::db_path(dir.path());
    let w = Writer::spawn(db.clone()).unwrap();
    // session row (FK)
    w.write(vec![ocserve_store::WriteOp::Sql {
        sql: "INSERT INTO session (id, project_id, directory, path, slug, title, version, time_created, time_updated) VALUES ('ses_m', 'global', '/w', 's', 's', 't', '1', 1, 1)".into(),
        params: vec![],
    }])
    .unwrap();
    // message written IMPORT-SHAPED: info lacks id + sessionID (source data blob)
    w.write(vec![ocserve_store::WriteOp::Sql {
        sql: "INSERT INTO msg (id, session_id, role, seq, time_created, info) VALUES ('msg_imported', 'ses_m', 'assistant', 1, 100, '{\"role\":\"assistant\",\"time\":{\"created\":100}}')".into(),
        params: vec![],
    }])
    .unwrap();
    // part written IMPORT-SHAPED: data = {type,text} only
    w.write(vec![ocserve_store::WriteOp::Sql {
        sql: "INSERT INTO msg_part (id, message_id, session_id, seq, type, byte_len, inline, blob_sha) VALUES ('prt_imported', 'msg_imported', 'ses_m', 1, 'text', 28, '{\"type\":\"text\",\"text\":\"hi\"}', NULL)".into(),
        params: vec![],
    }])
    .unwrap();
    // our-shaped row too (must stay correct — column-authoritative)
    w.write(vec![ocserve_store::WriteOp::Sql {
        sql: "INSERT INTO msg (id, session_id, role, seq, time_created, info) VALUES ('msg_own', 'ses_m', 'user', 2, 101, '{\"id\":\"msg_own\",\"sessionID\":\"ses_m\",\"role\":\"user\"}')".into(),
        params: vec![],
    }])
    .unwrap();
    drop(w); // flush

    let msgs = load_messages(&db, "ses_m", None).unwrap();
    assert_eq!(msgs.len(), 2);
    let (info0, parts0) = &msgs[0];
    // oc-remote requires id + sessionID on every message info (field error
    // was: Fields [id, sessionID] ... missing at path $[0].info)
    assert_eq!(info0["id"], "msg_imported", "column id merged");
    assert_eq!(info0["sessionID"], "ses_m", "column sessionID merged");
    assert_eq!(info0["role"], "assistant", "data fields preserved");
    // part fields: id / sessionID / messageID (parsePart contract)
    assert_eq!(parts0[0]["id"], "prt_imported");
    assert_eq!(parts0[0]["sessionID"], "ses_m");
    assert_eq!(parts0[0]["messageID"], "msg_imported");
    assert_eq!(parts0[0]["text"], "hi");
    // our own row: id stays (column-authoritative, same value)
    let (info1, _) = &msgs[1];
    assert_eq!(info1["id"], "msg_own");
    assert_eq!(info1["sessionID"], "ses_m");
}

// ---- W1: cursor page ordering + schema v5 ----

#[test]
fn page_messages_uses_tuple_order_with_same_ms_tiebreak() {
    use ocserve_store::{Writer, page_messages};
    let dir = tempfile::tempdir().unwrap();
    let db = ocserve_store::writer::db_path(dir.path());
    let w = Writer::spawn(db.clone()).unwrap();
    w.write(vec![ocserve_store::WriteOp::Sql {
        sql: "INSERT INTO session (id, project_id, directory, path, slug, title, version, time_created, time_updated) VALUES ('ses_t', 'global', '/w', 's', 's', 't', '1', 1, 1)".into(),
        params: vec![],
    }])
    .unwrap();
    // three messages: two share time_created (insertion order OPPOSITE to id
    // order — the A1 split-brain scenario), one older
    w.write(vec![
        ocserve_store::WriteOp::Sql {
            sql: "INSERT INTO msg (id, session_id, role, seq, time_created, info) VALUES ('msg_b', 'ses_t', 'user', 1, 1000, '{}')".into(),
            params: vec![],
        },
        ocserve_store::WriteOp::Sql {
            sql: "INSERT INTO msg (id, session_id, role, seq, time_created, info) VALUES ('msg_a', 'ses_t', 'user', 2, 1000, '{}')".into(),
            params: vec![],
        },
        ocserve_store::WriteOp::Sql {
            sql: "INSERT INTO msg (id, session_id, role, seq, time_created, info) VALUES ('msg_c', 'ses_t', 'user', 3, 2000, '{}')".into(),
            params: vec![],
        },
    ])
    .unwrap();
    drop(w);

    // newest-first walk: msg_c (2000), then same-ms pair by id DESC: msg_b, msg_a
    let (rows, more, next) = page_messages(&db, "ses_t", 10, None).unwrap();
    let ids: Vec<&str> = rows.iter().map(|(i, _)| i.as_str()).collect();
    assert_eq!(ids, ["msg_a", "msg_b", "msg_c"], "ASC after DESC window");
    assert!(!more && next.is_none());

    // page of 1 from the top: newest = msg_c; cursor = msg_c; older = tie pair
    let (rows1, more1, next1) = page_messages(&db, "ses_t", 1, None).unwrap();
    assert_eq!(rows1[0].0, "msg_c");
    assert!(more1);
    let cur = next1.unwrap();
    let (cid, ctime) = ocserve_store::decode_cursor(&cur).unwrap();
    assert_eq!((cid.as_str(), ctime), ("msg_c", 2000));
    let (rows2, more2, _) = page_messages(&db, "ses_t", 10, Some((&cid, ctime))).unwrap();
    let ids2: Vec<&str> = rows2.iter().map(|(i, _)| i.as_str()).collect();
    assert_eq!(ids2, ["msg_a", "msg_b"], "same-ms pair ordered by id ASC");
    assert!(!more2);
}

#[test]
fn fresh_schema_has_page_index_v5() {
    use ocserve_store::Writer;
    let dir = tempfile::tempdir().unwrap();
    let db = ocserve_store::writer::db_path(dir.path());
    let w = Writer::spawn(db.clone()).unwrap();
    drop(w);
    let conn = ocserve_store::pragma::open_reader(&db).unwrap();
    let has: i64 = conn
        .query_row(
            "SELECT count(*) FROM sqlite_master WHERE type='index' AND name='idx_msg_page'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(has, 1, "v5 page index must exist on fresh create");
    let ver: i64 = conn
        .pragma_query_value(None, "user_version", |r| r.get(0))
        .unwrap();
    assert_eq!(ver, ocserve_store::schema::SCHEMA_VERSION);
}

// ---- perf dive 2026-10-11: load_messages 64-chunk part prefetch ----

/// The chunked part prefetch must be byte-identical to the old per-message
/// query at BOTH sides of the 64-message chunk boundary (65 messages, parts
/// on #1, #64, #65), and blob-resolved parts must survive the batching.
/// Negative control: the pad-line removal was planted and this test went red
/// (last chunk fails to match 65th-message parts), then restored green.
#[test]
fn load_messages_chunk_boundary_and_blob_parts() {
    use ocserve_store::{BlobStore, Writer, load_messages};
    let dir = tempfile::tempdir().unwrap();
    let db = ocserve_store::writer::db_path(dir.path());
    let w = Writer::spawn(db.clone()).unwrap();
    w.write(vec![ocserve_store::WriteOp::Sql {
        sql: "INSERT INTO session (id, project_id, directory, path, slug, title, version, time_created, time_updated) VALUES ('ses_65', 'global', '/w', 's', 's', 't', '1', 1, 1)".into(),
        params: vec![],
    }])
    .unwrap();
    // 65 messages; parts on the first, the 64th, and the 65th (chunk edges)
    let mut ops = Vec::new();
    for i in 1..=65 {
        ops.push(ocserve_store::WriteOp::Sql {
            sql: format!(
                "INSERT INTO msg (id, session_id, role, seq, time_created, info) VALUES ('msg_c{i}', 'ses_65', 'user', {i}, {i}, '{{\"role\":\"user\",\"time\":{{\"created\":{i}}}}}')"
            ),
            params: vec![],
        });
    }
    for i in [1usize, 64, 65] {
        ops.push(ocserve_store::WriteOp::Sql {
            sql: format!(
                "INSERT INTO msg_part (id, message_id, session_id, seq, type, byte_len, inline, blob_sha) VALUES ('prt_c{i}', 'msg_c{i}', 'ses_65', 1, 'text', 30, '{{\"type\":\"text\",\"text\":\"inline-{i}\"}}', NULL)"
            ),
            params: vec![],
        });
    }
    w.write(ops).unwrap();
    // one blob-backed part on message 65 (spill path must resolve inside the chunk)
    let big = format!("{{\"type\":\"text\",\"text\":\"{}\"}}", "b".repeat(9000));
    let blobs = BlobStore::new(dir.path().join("blobs")).unwrap();
    let (sha, _, _) = blobs.put(big.as_bytes()).unwrap();
    w.write(vec![ocserve_store::WriteOp::Sql {
        sql: format!(
            "INSERT INTO msg_part (id, message_id, session_id, seq, type, byte_len, inline, blob_sha) VALUES ('prt_blob', 'msg_c65', 'ses_65', 2, 'text', {}, NULL, '{sha}')",
            big.len()
        ),
        params: vec![],
    }])
    .unwrap();
    drop(w);

    let msgs = load_messages(&db, "ses_65", None).unwrap();
    assert_eq!(msgs.len(), 65, "all messages returned in order");
    let ids: Vec<&str> = msgs.iter().map(|(i, _)| i["id"].as_str().unwrap()).collect();
    assert_eq!(ids[0], "msg_c1");
    assert_eq!(ids[63], "msg_c64");
    assert_eq!(ids[64], "msg_c65");
    assert_eq!(msgs[0].1.len(), 1, "first-message part survives");
    assert_eq!(msgs[0].1[0]["text"], "inline-1");
    assert_eq!(msgs[63].1.len(), 1, "chunk-boundary message 64 keeps its part");
    assert_eq!(msgs[63].1[0]["text"], "inline-64");
    assert_eq!(msgs[64].1.len(), 2, "last-chunk message keeps inline + blob parts");
    assert_eq!(msgs[64].1[0]["text"], "inline-65");
    assert_eq!(
        msgs[64].1[1]["text"].as_str().unwrap().len(),
        9000,
        "blob part resolved through the batched prefetch"
    );
    // messages without parts must have empty part vecs, not dropped rows
    assert!(msgs[1].1.is_empty());
}
