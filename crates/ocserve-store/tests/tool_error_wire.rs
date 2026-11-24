use ocserve_store::{MessageWalk, Writer, insert_message};
use serde_json::json;

fn seeded_db() -> (tempfile::TempDir, std::path::PathBuf, Writer) {
    let dir = tempfile::tempdir().unwrap();
    let db = ocserve_store::writer::db_path(dir.path());
    let conn = ocserve_store::pragma::open_writer(&db).unwrap();
    ocserve_store::schema::migrate(&conn).unwrap();
    drop(conn);
    let writer = Writer::spawn(db.clone()).unwrap();
    ocserve_store::insert_session(
        &writer,
        &json!({
            "id": "ses_error", "projectID": "global", "directory": "/w",
            "path": "ses_error", "slug": "ses_error", "title": "errors", "version": "1",
            "time": {"created": 1, "updated": 2},
        }),
    )
    .unwrap();
    insert_message(
        &writer,
        None,
        "ses_error",
        &json!({"id": "msg_error", "role": "assistant", "time": {"created": 10}}),
        &[
            json!({
                "id": "prt_error01", "type": "tool", "callID": "c1", "tool": "bash",
                "state": {"status": "error", "input": {}, "output": "legacy failure",
                           "title": "bash", "metadata": {"output": "legacy failure"},
                           "time": {"start": 1, "end": 2}}
            }),
            json!({
                "id": "prt_error03", "type": "tool", "callID": "c3", "tool": "bash",
                "state": {"status": "error", "input": {}, "error": "orig",
                           "output": "HOOKED", "metadata": {},
                           "time": {"start": 1, "end": 2}}
            }),
            json!({
                "id": "prt_error04", "type": "tool", "callID": "c4", "tool": "bash",
                "state": {"status": "error", "input": {}, "metadata": {},
                           "error": {"type": "unknown", "message": "structured"},
                           "time": {"start": 1, "end": 2}}
            }),
            json!({
                "id": "prt_error02", "type": "tool", "callID": "c2", "tool": "read",
                "state": {"status": "completed", "input": {}, "output": "ok",
                           "title": "read", "metadata": {}, "time": {"start": 3, "end": 4}}
            }),
        ],
    )
    .unwrap();
    (dir, db, writer)
}

fn assert_normalized(parts: &[serde_json::Value]) {
    assert_eq!(parts[0]["state"]["error"], "legacy failure");
    assert!(parts[0]["state"].get("output").is_none());
    assert!(parts[0]["state"].get("title").is_none());
    assert_eq!(parts[0]["state"]["metadata"]["output"], "legacy failure");
    // A hook replacement stored next to the original error: the replacement
    // wins and the completed-only field is dropped.
    assert_eq!(parts[1]["state"]["error"], "HOOKED");
    assert!(parts[1]["state"].get("output").is_none());
    // A structured v2 error is already valid and is never overwritten.
    assert_eq!(parts[2]["state"]["error"]["message"], "structured");
    // CONTROL: completed states keep their output and do not acquire error.
    assert_eq!(parts[3]["state"]["output"], "ok");
    assert!(parts[3]["state"].get("error").is_none());
}

#[test]
fn legacy_failed_tool_state_is_normalized_on_single_and_streamed_reads() {
    let (_dir, db, _writer) = seeded_db();

    let single = ocserve_store::message_by_id(&db, "ses_error", "msg_error")
        .unwrap()
        .unwrap();
    assert_normalized(single["parts"].as_array().unwrap());

    let stored = ocserve_store::load_messages(&db, "ses_error", None).unwrap();
    assert!(stored[0].1[0]["state"].get("error").is_none());
    assert_eq!(stored[0].1[0]["state"]["output"], "legacy failure");

    let mut chunks = Vec::new();
    ocserve_store::for_each_message_json(
        &db,
        "ses_error",
        MessageWalk::Seq { limit: None },
        |chunk| {
            chunks.push(chunk);
            Ok(())
        },
    )
    .unwrap();
    assert_eq!(chunks.len(), 1);
    let streamed: serde_json::Value = serde_json::from_str(&chunks[0]).unwrap();
    assert_normalized(streamed["parts"].as_array().unwrap());
}

/// PATCH round-trip (review of this PR): the read-boundary normalization must
/// not leak into storage. `load_messages` stays raw, the walk is normalized,
/// and `update_part` stores the client body verbatim — so a client that GETs a
/// legacy part and PATCHes it back cannot migrate the row by reading it.
#[test]
fn legacy_normalization_is_not_written_back() {
    let (dir, db, writer) = seeded_db();
    let blobs =
        std::sync::Arc::new(ocserve_store::BlobStore::new(dir.path().join("blobs")).unwrap());

    let stored = ocserve_store::load_messages(&db, "ses_error", None).unwrap();
    assert_eq!(
        stored[0].1[0]["state"]["output"], "legacy failure",
        "raw read must not normalize"
    );
    assert!(
        stored[0].1[0]["state"].get("error").is_none(),
        "raw read must not invent `error`"
    );

    let mut chunks = Vec::new();
    ocserve_store::for_each_message_json(&db, "ses_error", MessageWalk::Seq { limit: None }, |c| {
        chunks.push(c);
        Ok(())
    })
    .unwrap();
    let served: serde_json::Value = serde_json::from_str(&chunks[0]).unwrap();
    assert_eq!(
        served["parts"][0]["state"]["error"], "legacy failure",
        "served is normalized"
    );
    assert!(
        served["parts"][0]["state"].get("output").is_none(),
        "served drops output"
    );

    let mut body = served["parts"][0].clone();
    body["id"] = json!("prt_error01");
    ocserve_store::update_part(
        &writer,
        &blobs,
        "ses_error",
        "msg_error",
        "prt_error01",
        &body,
    )
    .unwrap();
    let after = ocserve_store::load_messages(&db, "ses_error", None).unwrap();
    assert_eq!(
        after[0].1[0]["state"]["error"], "legacy failure",
        "storage takes the client body verbatim: {}",
        after[0].1[0]
    );
}
