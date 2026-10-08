//! Preflight states (M6 audit fix): one reader answers seq+state; the
//! newest-message query runs ONLY when projection rows exist.
use ocserve_store::{PreflightState, Writer};

fn fixture(
    tag: &str,
) -> (
    tempfile::TempDir,
    std::path::PathBuf,
    std::sync::Arc<Writer>,
) {
    let dir = tempfile::tempdir().unwrap();
    let db = ocserve_store::writer::db_path(dir.path());
    let writer = std::sync::Arc::new(Writer::spawn(db.clone()).unwrap());
    ocserve_store::insert_session(
        &writer,
        &serde_json::json!({
            "id": format!("ses_{tag}"), "projectID": "global", "directory": "/w",
            "path": format!("ses_{tag}"), "slug": format!("ses_{tag}"),
            "title": tag, "version": "1", "time": {"created": 1, "updated": 2},
        }),
    )
    .unwrap();
    (dir, db, writer)
}

/// Anchor through the REAL insert path (compaction part → projection row).
fn insert_anchor(writer: &Writer, sid: &str) {
    ocserve_store::insert_message(
        writer,
        None,
        sid,
        &serde_json::json!({
            "id": "msg_anc", "sessionID": sid, "role": "user", "agent": "build",
            "time": {"created": 100},
        }),
        &[serde_json::json!({
            "id": "prt_anc", "type": "compaction", "auto": true, "overflow": false
        })],
    )
    .unwrap();
}

fn insert_summary(writer: &Writer, sid: &str, mid: &str, finish: &str) {
    ocserve_store::insert_message(
        writer,
        None,
        sid,
        &serde_json::json!({
            "id": mid, "sessionID": sid, "role": "assistant", "summary": true,
            "parentID": "msg_anc", "finish": finish, "time": {"created": 200},
        }),
        &[serde_json::json!({"id": "prt_sum", "type": "text", "text": "s"})],
    )
    .unwrap();
}

#[test]
fn ready_when_no_rows_even_if_last_message_is_summary() {
    let (_d, db, w) = fixture("ready");
    // row-gate proof: a finished summary EXISTS but no projection row —
    // must stay Ready (an always-query implementation would report
    // SummaryExit and break the hot-path rule)
    ocserve_store::insert_message(
        &w,
        None,
        "ses_ready",
        &serde_json::json!({
            "id": "msg_sum", "sessionID": "ses_ready", "role": "assistant",
            "summary": true, "parentID": "msg_none", "finish": "stop",
            "time": {"created": 100},
        }),
        &[serde_json::json!({"id": "prt_s", "type": "text", "text": "x"})],
    )
    .unwrap();
    let pre = ocserve_store::compaction_preflight(&db, "ses_ready").unwrap();
    assert_eq!(pre.seq, 1, "fresh session seq = 1");
    assert!(matches!(pre.state, PreflightState::Ready), "row-gated");
}

#[test]
fn pending_when_newest_anchor_unlinked() {
    let (_d, db, w) = fixture("pending");
    insert_anchor(&w, "ses_pending");
    let pre = ocserve_store::compaction_preflight(&db, "ses_pending").unwrap();
    match pre.state {
        PreflightState::Pending {
            anchor,
            auto,
            overflow,
        } => {
            assert_eq!(anchor, "msg_anc");
            assert!(auto, "auto flag round-trips");
            assert!(!overflow);
        }
        _ => panic!("expected Pending"),
    }
}

#[test]
fn summary_exit_when_row_linked_and_newest_is_that_summary() {
    let (_d, db, w) = fixture("exit");
    insert_anchor(&w, "ses_exit");
    insert_summary(&w, "ses_exit", "msg_sum", "stop");
    let pre = ocserve_store::compaction_preflight(&db, "ses_exit").unwrap();
    match pre.state {
        PreflightState::SummaryExit { info, parts } => {
            assert_eq!(info["id"], "msg_sum");
            assert!(!parts.is_empty());
        }
        _ => panic!("expected SummaryExit"),
    }
}

#[test]
fn unfinished_summary_finish_null_stays_ready() {
    let (_d, db, w) = fixture("unfin");
    insert_anchor(&w, "ses_unfin");
    insert_summary(&w, "ses_unfin", "msg_sum", "stop");
    // model the crash shell: summary linked but finish never set
    w.write(vec![ocserve_store::WriteOp::Sql {
        sql: "UPDATE msg SET info = json_set(info, '$.finish', NULL) WHERE id = 'msg_sum'".into(),
        params: vec![],
    }])
    .unwrap();
    let pre = ocserve_store::compaction_preflight(&db, "ses_unfin").unwrap();
    assert!(
        matches!(pre.state, PreflightState::Ready),
        "unfinished summary must not exit the loop"
    );
}
