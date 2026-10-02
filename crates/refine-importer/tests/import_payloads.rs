//! M3 payload import gates (TESTING §4 + §6): streaming counts, byte parity,
//! blob spill threshold, event/search_doc fill. Synthetic source mimics the
//! upstream schema (message/part/event columns verified against live DB).

use refine_importer::payload::import_payloads;
use rusqlite::{Connection, OpenFlags};

fn make_source(dir: &std::path::Path) -> std::path::PathBuf {
    let src = dir.join("source.db");
    let c = Connection::open(&src).unwrap();
    c.execute_batch(
        "CREATE TABLE session (id TEXT PRIMARY KEY, title TEXT NOT NULL DEFAULT '',
                                time_updated INTEGER NOT NULL);
         CREATE TABLE message (id TEXT PRIMARY KEY, session_id TEXT, time_created INTEGER,
                                time_updated INTEGER, data TEXT);
         CREATE TABLE part (id TEXT PRIMARY KEY, message_id TEXT, session_id TEXT,
                            time_created INTEGER, time_updated INTEGER, data TEXT);
         CREATE TABLE event (id INTEGER PRIMARY KEY, aggregate_id TEXT, seq INTEGER,
                             type TEXT, data TEXT);",
    )
    .unwrap();
    c.execute(
        "INSERT INTO session (id, title, time_updated) VALUES ('ses_1', 'Imported session', 1710000000000)",
        [],
    )
    .unwrap();
    for (i, role) in [(1i64, "user"), (2i64, "assistant")] {
        c.execute(
            "INSERT INTO message VALUES (?1, 'ses_1', ?2, ?2, ?3)",
            rusqlite::params![
                format!("msg_{i}"),
                1710000000000 + i,
                format!("{{\"id\":\"msg_{i}\",\"sessionID\":\"ses_1\",\"role\":\"{role}\"}}")
            ],
        )
        .unwrap();
    }
    let small = r#"{"id":"prt_a","type":"text","text":"hello"}"#;
    let big = format!(
        r#"{{"id":"prt_big","type":"text","text":"{}"}}"#,
        "x".repeat(12_000)
    );
    for (pid, mid, data) in [
        ("prt_a", "msg_1", small.to_string()),
        ("prt_big", "msg_2", big),
    ] {
        c.execute(
            "INSERT INTO part VALUES (?1, ?2, 'ses_1', 1, 1, ?3)",
            rusqlite::params![pid, mid, data],
        )
        .unwrap();
    }
    for (i, t) in [(1, "message.updated"), (2, "session.idle")] {
        c.execute(
            "INSERT INTO event (aggregate_id, seq, type, data) VALUES ('ses_1', ?1, ?2, ?3)",
            rusqlite::params![i, t, format!("{{\"seq\":{i}}}")],
        )
        .unwrap();
    }
    drop(c);
    src
}

#[test]
fn import_counts_byte_parity_and_blob_spill() {
    let dir = tempfile::tempdir().unwrap();
    let src = make_source(dir.path());
    let target = dir.path().join("refine.db");
    {
        let c = Connection::open(&target).unwrap();
        refine_store::schema::migrate(&c).unwrap();
        // metadata phase normally creates the session row (msg FK target)
        c.execute(
            "INSERT INTO session (id, project_id, directory, path, slug, title, version, time_created, time_updated)
             VALUES ('ses_1', 'global', '/tmp', 'ses_1', 'ses_1', 'Imported session', '1', 1710000000000, 1710000000000)",
            [],
        ).unwrap();
    }
    let stats = import_payloads(&src, &target, &["ses_1".to_string()]).unwrap();
    assert_eq!(stats.messages, 2, "two source messages");
    assert_eq!(stats.parts, 2, "two source parts");
    assert_eq!(stats.parts_blobbed, 1, "only the >8KB part spills to blobs");
    assert_eq!(stats.events, 2);
    assert_eq!(stats.search_docs, 1);

    let dst = Connection::open_with_flags(&target, OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();
    let src_c = Connection::open_with_flags(&src, OpenFlags::SQLITE_OPEN_READ_ONLY).unwrap();

    // byte parity: inline part identical to source bytes
    let src_data: String = src_c
        .query_row("SELECT data FROM part WHERE id='prt_a'", [], |r| r.get(0))
        .unwrap();
    let inline: Option<String> = dst
        .query_row("SELECT inline FROM msg_part WHERE id='prt_a'", [], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(
        inline.as_deref(),
        Some(src_data.as_str()),
        "byte-equal inline"
    );

    // big part → blob, byte_len = source length, content byte-equal
    let (sha, byte_len): (Option<String>, i64) = dst
        .query_row(
            "SELECT blob_sha, byte_len FROM msg_part WHERE id='prt_big'",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    let sha = sha.expect("big part must be blobbed");
    let src_big: String = src_c
        .query_row("SELECT data FROM part WHERE id='prt_big'", [], |r| r.get(0))
        .unwrap();
    assert_eq!(byte_len as usize, src_big.len(), "byte_len = source length");
    let blobs = refine_store::BlobStore::new(dir.path().join("blobs")).unwrap();
    let got = String::from_utf8_lossy(&blobs.get(&sha, byte_len as u64).unwrap()).into_owned();
    assert_eq!(got, src_big, "blob content byte-equal to source");

    // event payload preserved verbatim
    let payloads: Vec<String> = dst
        .prepare("SELECT payload FROM event ORDER BY seq")
        .unwrap()
        .query_map([], |r| r.get(0))
        .unwrap()
        .flatten()
        .collect();
    assert_eq!(payloads, vec!["{\"seq\":1}", "{\"seq\":2}"]);

    // search_doc title filled from session title
    let title: String = dst
        .query_row("SELECT title FROM search_doc LIMIT 1", [], |r| r.get(0))
        .unwrap();
    assert_eq!(title, "Imported session");

    // FTS external-content index responds (rebuild ran)
    let hits: i64 = dst
        .query_row(
            "SELECT count(*) FROM search_fts WHERE search_fts MATCH 'Imported'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(hits, 1, "FTS matches the imported title");
}
