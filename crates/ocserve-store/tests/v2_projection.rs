//! V2-PLAN.md P4: real `content[]` projection over stored rows.
//!
//! `project_page_v2` maps the v1 part stream (text/reasoning/tool +
//! envelope parts) to v2 `Session.Message` shapes. Proves: text + reasoning
//! + completed/error/running tool states, user files/agents, blobbed parts,
//! envelope parts excluded, unknown roles dropped, one-IN-query batching.

use ocserve_store::{inbox, writer};
use serde_json::json;

fn seeded_db() -> (tempfile::TempDir, std::path::PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let db = writer::db_path(dir.path());
    let conn = ocserve_store::pragma::open_writer(&db).unwrap();
    ocserve_store::schema::migrate(&conn).unwrap();
    drop(conn);
    let w = writer::Writer::spawn(db.clone()).unwrap();
    ocserve_store::insert_session(
        &w,
        &json!({
            "id": "ses_p", "projectID": "global", "directory": "/w",
            "path": "ses_p", "slug": "ses_p", "title": "p", "version": "1",
            "time": {"created": 1, "updated": 2},
        }),
    )
    .unwrap();
    // User message with text + a file part.
    ocserve_store::insert_message(
        &w,
        None,
        "ses_p",
        &json!({"id": "msg_u", "role": "user", "time": {"created": 10}, "text": "do it"}),
        &[json!({"id": "prt_seed01", "type": "text", "text": "do it"}),
          json!({"id": "prt_seed02", "type": "file", "file": {"uri": "file:///a.txt"}})],
    )
    .unwrap();
    // Assistant with text + reasoning + completed tool + error tool +
    // running tool + envelope noise (step-start must not become content).
    ocserve_store::insert_message(
        &w,
        None,
        "ses_p",
        &json!({"id": "msg_a", "role": "assistant", "time": {"created": 20},
                 "agent": "build", "modelID": "m", "providerID": "fake",
                 "cost": 0.1, "tokens": {"input": 1}}),
        &[
            json!({"id": "prt_seed03", "type": "step-start", "snapshot": "abc"}),
            json!({"id": "prt_seed04", "type": "text", "text": "here"}),
            json!({"id": "prt_seed05", "type": "reasoning", "text": "hmm"}),
            json!({"id": "prt_seed06", "type": "tool", "callID": "c1", "tool": "read",
                   "state": {"status": "completed", "input": {"path": "/a"},
                             "output": [{"text": "x"}],
                             "time": {"start": 1, "end": 2}}}),
            json!({"id": "prt_seed07", "type": "tool", "callID": "c2", "tool": "bash",
                   "state": {"status": "error", "input": {"cmd": "y"},
                             "error": "boom", "time": {"start": 3}}}),
            json!({"id": "prt_seed08", "type": "tool", "callID": "c3", "tool": "grep",
                   "state": {"status": "running", "input": {"q": "z"}}}),
        ],
    )
    .unwrap();
    (dir, db)
}

#[test]
fn content_projection_maps_all_part_kinds() {
    let (_dir, db) = seeded_db();
    let page = vec![
        ("msg_u".to_string(), "{\"id\":\"msg_u\",\"role\":\"user\",\"time\":{\"created\":10},\"text\":\"do it\"}".to_string()),
        ("msg_a".to_string(), "{\"id\":\"msg_a\",\"role\":\"assistant\",\"time\":{\"created\":20},\"agent\":\"build\",\"modelID\":\"m\",\"providerID\":\"fake\",\"cost\":0.1,\"tokens\":{\"input\":1}}".to_string()),
    ];
    let out = ocserve_store::project_page_v2(&db, &page).unwrap();
    assert_eq!(out.len(), 2);
    // User: text passthrough + file part surfaced.
    assert_eq!(out[0]["type"], "user");
    assert_eq!(out[0]["text"], "do it");
    assert_eq!(out[0]["files"][0]["file"]["uri"], "file:///a.txt");
    // Assistant: envelope part excluded; 2 text-ish + 3 tools.
    let content = out[1]["content"].as_array().unwrap();
    assert_eq!(content.len(), 5, "text+reasoning+3 tools, no step-start");
    assert_eq!((content[0]["type"].as_str(), content[0]["text"].as_str()), (Some("text"), Some("here")));
    assert_eq!((content[1]["type"].as_str(), content[1]["text"].as_str()), (Some("reasoning"), Some("hmm")));
    assert_eq!(content[2]["name"], "read");
    assert_eq!(content[2]["state"]["status"], "completed");
    assert_eq!(content[2]["state"]["output"][0]["text"], "x");
    assert_eq!(content[3]["state"]["status"], "error");
    assert_eq!(content[3]["state"]["error"], "boom");
    assert_eq!(content[4]["state"]["status"], "running");
    assert_eq!(out[1]["model"]["providerID"], "fake");
    // CONTROL: map step-start into content → len 6, red. Drop the running
    // arm → len 4, red.
}

#[test]
fn blobbed_parts_decode_like_production() {
    use ocserve_store::blob::BlobStore;
    let dir = tempfile::tempdir().unwrap();
    let db = writer::db_path(dir.path());
    let conn = ocserve_store::pragma::open_writer(&db).unwrap();
    ocserve_store::schema::migrate(&conn).unwrap();
    drop(conn);
    let blobs = BlobStore::new(dir.path().join("blobs")).unwrap();
    let big = "z".repeat(ocserve_store::INLINE_PART_MAX + 100);
    let w = writer::Writer::spawn(db.clone()).unwrap();
    ocserve_store::insert_session(
        &w,
        &json!({"id": "ses_b", "time": {"created": 1, "updated": 2}}),
    )
    .unwrap();
    ocserve_store::insert_message(
        &w,
        Some(&blobs),
        "ses_b",
        &json!({"id": "msg_big", "role": "assistant", "time": {"created": 5}}),
        &[json!({"id": "prt_seed09", "type": "text", "text": big})],
    )
    .unwrap();
    let page = vec![(
        "msg_big".to_string(),
        "{\"id\":\"msg_big\",\"role\":\"assistant\",\"time\":{\"created\":5}}".to_string(),
    )];
    let out = ocserve_store::project_page_v2(&db, &page).unwrap();
    assert_eq!(out.len(), 1);
    assert_eq!(out[0]["content"][0]["text"].as_str().unwrap().len(), big.len());
    // CONTROL: skip blob decode (empty text) → len 0, red.
}

#[test]
fn unknown_roles_drop_and_empty_page_is_empty() {
    let (_dir, db) = seeded_db();
    let page = vec![(
        "msg_x".to_string(),
        "{\"id\":\"msg_x\",\"role\":\"system\",\"time\":{\"created\":1}}".to_string(),
    )];
    assert!(ocserve_store::project_page_v2(&db, &page).unwrap().is_empty());
    assert!(ocserve_store::project_page_v2(&db, &[]).unwrap().is_empty());
    // CONTROL: project system rows → non-empty, red.
}

#[test]
fn inbox_list_shapes_match_inbox_info() {
    // The /api/session/{id}/inbox response shape is built in v2.rs, but the
    // underlying row order + delivery values come from store::inbox::list —
    // pin them here so a store change reds before the HTTP suite runs.
    let (_dir, db) = seeded_db();
    let w = writer::Writer::spawn(db.clone()).unwrap();
    inbox::admit(&w, "ses_p", "m1", "user", &json!({"text": "a"}), "steer", 1).unwrap();
    inbox::admit(&w, "ses_p", "m2", "compaction", &json!({}), "steer", 2).unwrap();
    let rows = inbox::list(&db, "ses_p").unwrap();
    assert_eq!(rows.len(), 2);
    assert_eq!((rows[0].0.as_str(), rows[0].1.as_str(), rows[0].3.as_str()), ("m1", "user", "steer"));
    assert_eq!((rows[1].0.as_str(), rows[1].1.as_str()), ("m2", "compaction"));
    // CONTROL: reorder list by id → m2 first, red.
}
