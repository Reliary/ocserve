//! Bounded prompt-load window (`prompt_window`).
//!
//! The window is only a candidate; `ocserve-core` proves equivalence before
//! trusting it. These tests pin the two things the store must get right:
//!
//! 1. with no compaction the window is a plain `seq` suffix (or 0 when the
//!    session fits the budget), never a timestamp-ordered slice;
//! 2. with a completed compaction the window starts at the verbatim tail (or
//!    the anchor when no tail was recorded), and every shape the filter cannot
//!    resolve degrades to the full load instead of guessing.
//!
//! Negative control (run during development): return `anchor_seq` instead of
//! `tail_seq` and `compaction_window_starts_at_tail` fails; return `(seq,
//! false)` for the no-tail case and `compaction_without_tail_keeps_anchor`
//! fails.

use ocserve_store::{BlobStore, Writer};
use serde_json::json;

struct Fx {
    _dir: tempfile::TempDir,
    db: std::path::PathBuf,
    blobs: std::sync::Arc<BlobStore>,
    writer: std::sync::Arc<Writer>,
    sid: String,
}

fn fixture(tag: &str) -> Fx {
    let dir = tempfile::tempdir().unwrap();
    let db = ocserve_store::writer::db_path(dir.path());
    let writer = std::sync::Arc::new(Writer::spawn(db.clone()).unwrap());
    let blobs = std::sync::Arc::new(BlobStore::new(dir.path().join("blobs")).unwrap());
    let sid = format!("ses_{tag}");
    ocserve_store::insert_session(
        &writer,
        &json!({
            "id": sid, "projectID": "global", "directory": "/w",
            "path": sid, "slug": sid, "title": tag, "version": "1",
            "time": {"created": 1, "updated": 2},
        }),
    )
    .unwrap();
    Fx {
        _dir: dir,
        db,
        blobs,
        writer,
        sid,
    }
}

fn msg(fx: &Fx, id: &str, role: &str, text: &str) {
    ocserve_store::insert_message(
        &fx.writer,
        Some(&fx.blobs),
        &fx.sid,
        &json!({
            "id": id, "sessionID": fx.sid, "role": role, "agent": "build",
            "time": {"created": 10},
        }),
        &[json!({"id": format!("{id}_p"), "type": "text", "text": text})],
    )
    .unwrap();
}

fn anchor(fx: &Fx, id: &str) {
    ocserve_store::insert_message(
        &fx.writer,
        Some(&fx.blobs),
        &fx.sid,
        &json!({
            "id": id, "sessionID": fx.sid, "role": "user", "agent": "build",
            "time": {"created": 10},
        }),
        &[json!({
            "id": format!("{id}_p"), "type": "compaction",
            "auto": true, "overflow": false,
        })],
    )
    .unwrap();
}

fn summary(fx: &Fx, id: &str, anchor: &str) {
    ocserve_store::insert_message(
        &fx.writer,
        Some(&fx.blobs),
        &fx.sid,
        &json!({
            "id": id, "sessionID": fx.sid, "role": "assistant",
            "summary": true, "parentID": anchor, "finish": "stop",
            "time": {"created": 10},
        }),
        &[json!({"id": format!("{id}_p"), "type": "text", "text": "summary"})],
    )
    .unwrap();
}

fn set_tail(fx: &Fx, anchor: &str, tail_msg: &str) {
    let part_id = format!("{anchor}_p");
    ocserve_store::update_part(
        &fx.writer,
        &fx.blobs,
        &fx.sid,
        anchor,
        &part_id,
        &json!({
            "id": part_id, "type": "compaction", "auto": true,
            "overflow": false, "tail_start_id": tail_msg,
        }),
    )
    .unwrap();
}

fn seq_of(fx: &Fx, id: &str) -> i64 {
    let conn =
        rusqlite::Connection::open_with_flags(&fx.db, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
            .unwrap();
    conn.query_row(
        "SELECT seq FROM msg WHERE session_id = ?1 AND id = ?2",
        (fx.sid.as_str(), id),
        |r| r.get(0),
    )
    .unwrap()
}

#[test]
fn small_session_needs_no_window() {
    let fx = fixture("small");
    msg(&fx, "msg_a", "user", "hi");
    msg(&fx, "msg_b", "assistant", "hello");
    // Budget far above the payload: the walk exhausts the session.
    let (min_seq, authoritative) = ocserve_store::prompt_window(&fx.db, &fx.sid, 8 << 20).unwrap();
    assert_eq!(min_seq, 0, "whole session is cheaper than a window");
    assert!(!authoritative);
}

#[test]
fn budget_tail_is_a_seq_suffix() {
    let fx = fixture("suffix");
    for i in 0..12 {
        msg(&fx, &format!("msg_{i}"), "user", &"x".repeat(4096));
    }
    // The selector targets ~3× the budget in raw payload, which lands inside
    // this data set (each message is ~4 KiB of text).
    let (min_seq, authoritative) = ocserve_store::prompt_window(&fx.db, &fx.sid, 8 * 1024).unwrap();
    let last = seq_of(&fx, "msg_11");
    assert!(!authoritative);
    assert!(min_seq > 0 && min_seq <= last, "got min_seq={min_seq}");
    // A suffix, by definition: it must exclude the oldest message.
    assert!(
        min_seq > seq_of(&fx, "msg_0"),
        "window must not be the whole session"
    );
}

#[test]
fn compaction_window_starts_at_tail() {
    let fx = fixture("tail");
    msg(&fx, "msg_keep", "user", "verbatim tail");
    anchor(&fx, "msg_anc");
    summary(&fx, "msg_sum", "msg_anc");
    set_tail(&fx, "msg_anc", "msg_keep");
    msg(&fx, "msg_after", "user", "newest");

    let (min_seq, authoritative) = ocserve_store::prompt_window(&fx.db, &fx.sid, 8 << 20).unwrap();
    assert!(
        authoritative,
        "a completed compaction must be authoritative"
    );
    assert_eq!(
        min_seq,
        seq_of(&fx, "msg_keep"),
        "window must start at the verbatim tail (the filter's stop point)"
    );
}

#[test]
fn compaction_without_tail_keeps_anchor() {
    let fx = fixture("notail");
    msg(&fx, "msg_old", "user", "before");
    anchor(&fx, "msg_anc");
    summary(&fx, "msg_sum", "msg_anc");
    msg(&fx, "msg_after", "user", "newest");

    let (min_seq, authoritative) = ocserve_store::prompt_window(&fx.db, &fx.sid, 8 << 20).unwrap();
    assert!(authoritative);
    assert_eq!(
        min_seq,
        seq_of(&fx, "msg_anc"),
        "without a tail the filter keeps [anchor..], so the window starts there"
    );
}

#[test]
fn missing_tail_message_degrades_to_full_load() {
    let fx = fixture("gone");
    msg(&fx, "msg_keep", "user", "tail");
    anchor(&fx, "msg_anc");
    summary(&fx, "msg_sum", "msg_anc");
    set_tail(&fx, "msg_anc", "msg_never_existed");
    msg(&fx, "msg_after", "user", "newest");

    let (min_seq, authoritative) = ocserve_store::prompt_window(&fx.db, &fx.sid, 8 << 20).unwrap();
    assert_eq!(min_seq, 0, "a dangling tail must not be guessed");
    assert!(!authoritative);
}

#[test]
fn unfinished_summary_is_not_a_boundary() {
    let fx = fixture("unfinished");
    msg(&fx, "msg_old", "user", "before");
    anchor(&fx, "msg_anc");
    // No `finish`: the reverse walk does not treat this as completed.
    ocserve_store::insert_message(
        &fx.writer,
        Some(&fx.blobs),
        &fx.sid,
        &json!({
            "id": "msg_sum", "sessionID": fx.sid, "role": "assistant",
            "summary": true, "parentID": "msg_anc",
            "time": {"created": 10},
        }),
        &[json!({"id": "msg_sum_p", "type": "text", "text": "partial"})],
    )
    .unwrap();
    set_tail(&fx, "msg_anc", "msg_old");

    let (min_seq, authoritative) = ocserve_store::prompt_window(&fx.db, &fx.sid, 8 << 20).unwrap();
    assert_eq!(min_seq, 0, "only a completed summary is a boundary");
    assert!(!authoritative);
}

#[test]
fn error_summary_is_not_a_boundary() {
    let fx = fixture("errored");
    msg(&fx, "msg_old", "user", "before");
    anchor(&fx, "msg_anc");
    ocserve_store::insert_message(
        &fx.writer,
        Some(&fx.blobs),
        &fx.sid,
        &json!({
            "id": "msg_sum", "sessionID": fx.sid, "role": "assistant",
            "summary": true, "parentID": "msg_anc", "finish": "stop",
            "error": {"name": "boom"},
            "time": {"created": 10},
        }),
        &[json!({"id": "msg_sum_p", "type": "text", "text": "failed"})],
    )
    .unwrap();
    set_tail(&fx, "msg_anc", "msg_old");

    let (min_seq, authoritative) = ocserve_store::prompt_window(&fx.db, &fx.sid, 8 << 20).unwrap();
    assert_eq!(min_seq, 0, "an errored summary is not completed");
    assert!(!authoritative);
}

#[test]
fn tail_after_anchor_is_not_treated_as_authoritative() {
    // A tail pointing forward is never reached before the reverse walk
    // exhausts the session, so the filter returns everything. Claiming a
    // window here would silently shrink the prompt.
    let fx = fixture("forward");
    msg(&fx, "msg_old", "user", "before");
    anchor(&fx, "msg_anc");
    summary(&fx, "msg_sum", "msg_anc");
    msg(&fx, "msg_late", "user", "after the anchor");
    set_tail(&fx, "msg_anc", "msg_late");

    let (min_seq, authoritative) = ocserve_store::prompt_window(&fx.db, &fx.sid, 8 << 20).unwrap();
    assert_eq!(
        (min_seq, authoritative),
        (0, false),
        "a forward tail makes the filter keep everything; only the full load is safe"
    );
}
