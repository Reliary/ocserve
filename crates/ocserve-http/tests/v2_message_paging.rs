//! K-V2-MESSAGE-PAGE: the v2 message list must advance on `?cursor=`.
//!
//! Regression for the 2026-11-11 Paseo heap death: `GET
//! /api/session/{id}/message` accepted `?cursor=` but passed `None` to
//! `page_messages`, so every page returned the SAME first window and the SAME
//! `cursor.next`. Paseo walks the list page by page; it appended the repeated
//! page to its timeline until the daemon's JavaScript heap reached ~3.4 GB and
//! aborted. The global `timelineStats` stayed flat, which is why the leak
//! looked like it had no event source.
//!
//! The negative control is the second-page assertion: on the pre-fix code page
//! 2 equals page 1 (same ids, same next cursor), so these assertions fail
//! rather than pass vacuously.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use ocserve_http::{AppState, LlmRegistry, Payloads, Wires};
use serde_json::Value;
use tower::ServiceExt;

fn next_fixture() -> u64 {
    static N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    N.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
}

fn state_v2() -> (std::sync::Arc<AppState>, axum::Router) {
    let dir = std::env::temp_dir().join(format!(
        "ocserve-v2-page-{}-{}",
        std::process::id(),
        next_fixture()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let db = ocserve_store::writer::db_path(&dir);
    let writer = std::sync::Arc::new(ocserve_store::Writer::spawn(db.clone()).unwrap());
    let blobs = std::sync::Arc::new(ocserve_store::BlobStore::new(dir.join("blobs")).unwrap());
    let llm = LlmRegistry {
        modalities: Default::default(),
        limits: std::collections::HashMap::new(),
        endpoints: [("fake".into(), ("http://127.0.0.1:9".into(), String::new()))]
            .into_iter()
            .collect(),
        pricing: Default::default(),
        default_model: ("fake".into(), "m".into()),
        systems: Default::default(),
        default_agent: "build".into(),
    };
    let st = AppState::with_wiring_and_v2(
        None,
        Payloads::default(),
        Wires {
            db,
            blobs,
            writer,
            llm,
        },
        true,
    );
    let app = ocserve_http::router(st.clone());
    (st, app)
}

fn seed_session(st: &AppState, sid: &str) {
    ocserve_store::insert_session(
        &st.writer,
        &serde_json::json!({
            "id": sid, "projectID": "global", "directory": "/w", "path": sid,
            "slug": sid, "title": sid, "version": "1",
            "time": {"created": 1, "updated": 2},
        }),
    )
    .unwrap();
}

fn seed_msgs(st: &AppState, sid: &str, n: usize) {
    let mut ops: Vec<ocserve_store::WriteOp> = Vec::new();
    for i in 0..n {
        ops.push(ocserve_store::WriteOp::Sql {
            sql: "INSERT INTO msg (id, session_id, role, seq, time_created, info) VALUES (?1, ?2, 'user', ?3, ?4, ?5)".into(),
            params: vec![
                format!("msg_{i:04}").into(),
                sid.into(),
                ((i + 1) as i64).into(),
                (1_700_000_000_000i64 + i as i64).into(),
                format!("{{\"id\":\"msg_{i:04}\",\"sessionID\":\"{sid}\",\"role\":\"user\"}}").into(),
            ],
        });
    }
    st.writer.write(ops).unwrap();
}

async fn get(app: &axum::Router, uri: &str) -> (StatusCode, Value) {
    let resp = app
        .clone()
        .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = resp.status();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

fn ids(page: &Value) -> Vec<String> {
    page["data"]
        .as_array()
        .expect("data array")
        .iter()
        .map(|m| m["id"].as_str().expect("message id").to_string())
        .collect()
}

fn next_cursor(page: &Value) -> Option<String> {
    page["cursor"]["next"].as_str().map(str::to_string)
}

#[tokio::test]
async fn cursor_advances_pages_and_terminates() {
    let (st, app) = state_v2();
    seed_session(&st, "ses_p");
    seed_msgs(&st, "ses_p", 12);

    // Default order is DESC: the newest window first (measured against 2.2.5).
    let (s1, p1) = get(&app, "/api/session/ses_p/message?limit=5").await;
    assert_eq!(s1, StatusCode::OK, "{p1}");
    assert_eq!(
        ids(&p1),
        ["msg_0011", "msg_0010", "msg_0009", "msg_0008", "msg_0007"],
        "first page is the newest window, DESC"
    );
    let c1 = next_cursor(&p1).expect("first page has a next cursor");

    let (s2, p2) = get(
        &app,
        &format!("/api/session/ses_p/message?limit=5&cursor={c1}"),
    )
    .await;
    assert_eq!(s2, StatusCode::OK, "{p2}");
    assert_ne!(
        ids(&p2),
        ids(&p1),
        "NEGATIVE CONTROL: a cursor must not return the first page again"
    );
    assert_eq!(
        ids(&p2),
        ["msg_0006", "msg_0005", "msg_0004", "msg_0003", "msg_0002"],
        "second page is strictly older than the cursor"
    );
    let c2 = next_cursor(&p2).expect("second page has a next cursor");
    assert_ne!(c1, c2, "the next cursor must advance");

    let (s3, p3) = get(
        &app,
        &format!("/api/session/ses_p/message?limit=5&cursor={c2}"),
    )
    .await;
    assert_eq!(s3, StatusCode::OK, "{p3}");
    assert_eq!(
        ids(&p3),
        ["msg_0001", "msg_0000"],
        "last page has the remainder"
    );

    // Upstream still returns a cursor on the final non-empty page; the walk
    // ends on an EMPTY page, not on a null cursor (measured against 2.2.5).
    let c3 = next_cursor(&p3).expect("final non-empty page carries a cursor");
    let (s4, p4) = get(
        &app,
        &format!("/api/session/ses_p/message?limit=5&cursor={c3}"),
    )
    .await;
    assert_eq!(s4, StatusCode::OK, "{p4}");
    assert!(ids(&p4).is_empty(), "the walk terminates on an empty page");
    assert_eq!(
        next_cursor(&p4),
        None,
        "the empty page carries an explicit null cursor"
    );

    // Every message appears exactly once across the walk.
    let mut all = ids(&p1);
    all.extend(ids(&p2));
    all.extend(ids(&p3));
    all.sort();
    all.dedup();
    assert_eq!(all.len(), 12, "no duplicates, no gaps: {all:?}");
}

#[tokio::test]
async fn asc_order_and_backward_walk() {
    let (st, app) = state_v2();
    seed_session(&st, "ses_p");
    seed_msgs(&st, "ses_p", 6);

    let (s, p1) = get(&app, "/api/session/ses_p/message?limit=2&order=asc").await;
    assert_eq!(s, StatusCode::OK, "{p1}");
    assert_eq!(
        ids(&p1),
        ["msg_0000", "msg_0001"],
        "order=asc starts at the oldest message"
    );
    // The order travels inside the cursor: paging without repeating `order`
    // keeps walk direction and ordering.
    let c1 = next_cursor(&p1).expect("next cursor");
    let (_, p2) = get(
        &app,
        &format!("/api/session/ses_p/message?limit=2&cursor={c1}"),
    )
    .await;
    assert_eq!(
        ids(&p2),
        ["msg_0002", "msg_0003"],
        "asc paging moves forward"
    );

    // Walking BACK from p2's `previous` returns p1 again, in asc order.
    // (Measured: walking back from the FIRST window's `previous` is an empty
    // page upstream too — there is nothing before the oldest message.)
    let back = p2["cursor"]["previous"].as_str().expect("previous cursor");
    let (_, pb) = get(
        &app,
        &format!("/api/session/ses_p/message?limit=2&cursor={back}"),
    )
    .await;
    assert_eq!(ids(&pb), ids(&p1), "backward walk returns the prior window");

    let first_prev = p1["cursor"]["previous"].as_str().expect("previous cursor");
    let (_, pe) = get(
        &app,
        &format!("/api/session/ses_p/message?limit=2&cursor={first_prev}"),
    )
    .await;
    assert!(ids(&pe).is_empty(), "nothing precedes the oldest window");
}

#[tokio::test]
async fn order_and_type_are_load_bearing() {
    let (st, app) = state_v2();
    seed_session(&st, "ses_p");
    seed_msgs(&st, "ses_p", 4);
    // one synthetic user row among the plain ones
    st.writer
        .write(vec![ocserve_store::WriteOp::Sql {
            sql: "INSERT INTO msg (id, session_id, role, seq, time_created, info) VALUES (?1, ?2, 'user', ?3, ?4, ?5)".into(),
            params: vec![
                "msg_syn0".into(),
                "ses_p".into(),
                9i64.into(),
                1_700_000_000_500i64.into(),
                "{\"id\":\"msg_syn0\",\"role\":\"user\",\"synthetic\":true}".into(),
            ],
        }])
        .unwrap();

    // type filters BEFORE the limit: a full page of that type, not a short one.
    let (s, t) = get(&app, "/api/session/ses_p/message?limit=10&type=synthetic").await;
    assert_eq!(s, StatusCode::OK, "{t}");
    assert_eq!(
        ids(&t),
        ["msg_syn0"],
        "type=synthetic selects only that row"
    );
    assert_eq!(t["data"][0]["type"], "synthetic", "and reports the v2 type");

    let (s, u) = get(&app, "/api/session/ses_p/message?limit=3&type=user").await;
    assert_eq!(s, StatusCode::OK, "{u}");
    assert_eq!(
        ids(&u),
        ["msg_0003", "msg_0002", "msg_0001"],
        "a filtered page is still a full page (4 plain rows, not the synthetic one)"
    );

    // Unread parameters are the regression: a bogus order or type must 400,
    // never be silently ignored.
    let (s, e) = get(&app, "/api/session/ses_p/message?order=sideways").await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    assert_eq!(e["_tag"], "InvalidRequestError", "{e}");
    assert_eq!(e["kind"], "Query", "{e}");

    let (s, e) = get(&app, "/api/session/ses_p/message?type=bogus").await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    assert_eq!(e["kind"], "Query", "{e}");
    assert!(
        e["message"]
            .as_str()
            .unwrap_or("")
            .contains("agent-switched"),
        "the error lists the contract's type union: {e}"
    );

    let (s, e) = get(&app, "/api/session/ses_p/message?limit=0").await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "{e}");
    let (s, e) = get(&app, "/api/session/ses_p/message?limit=201").await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "{e}");
    let (s, e) = get(&app, "/api/session/ses_p/message?limit=abc").await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "{e}");
}

#[tokio::test]
async fn cursor_and_order_together_are_an_error() {
    let (st, app) = state_v2();
    seed_session(&st, "ses_p");
    seed_msgs(&st, "ses_p", 2);
    let (_, p) = get(&app, "/api/session/ses_p/message?limit=1").await;
    let c = next_cursor(&p).expect("cursor");

    // Measured: the combination is rejected BEFORE the cursor is decoded, so
    // even a garbage cursor reports the combination.
    for cur in [c.as_str(), "garbage"] {
        let (s, b) = get(
            &app,
            &format!("/api/session/ses_p/message?limit=1&order=desc&cursor={cur}"),
        )
        .await;
        assert_eq!(s, StatusCode::BAD_REQUEST, "{b}");
        assert_eq!(b["_tag"], "InvalidCursorError", "{b}");
        assert_eq!(b["message"], "Cursor cannot be combined with order", "{b}");
    }
}

#[tokio::test]
async fn unknown_cursor_anchor_is_an_empty_page() {
    let (st, app) = state_v2();
    seed_session(&st, "ses_p");
    seed_msgs(&st, "ses_p", 2);
    let bogus = ocserve_store::V2MessageCursor {
        id: "msg_doesnotexist".into(),
        order: ocserve_store::MessageOrder::Desc,
        direction: ocserve_store::CursorDirection::Next,
    }
    .encode();
    let (s, b) = get(&app, &format!("/api/session/ses_p/message?cursor={bogus}")).await;
    assert_eq!(s, StatusCode::OK, "an unknown anchor is not an error: {b}");
    assert!(ids(&b).is_empty(), "{b}");
    assert_eq!(next_cursor(&b), None, "{b}");
}

#[tokio::test]
async fn invalid_cursor_is_a_typed_400() {
    let (st, app) = state_v2();
    seed_session(&st, "ses_p");
    seed_msgs(&st, "ses_p", 1);

    let (s, body) = get(&app, "/api/session/ses_p/message?cursor=garbage").await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    assert_eq!(body["_tag"], "InvalidCursorError", "{body}");
    assert_eq!(body["message"], "Invalid cursor", "{body}");

    // A v1-shaped {id,time} cursor is not a v2 cursor and must not be accepted.
    let v1 = ocserve_store::encode_cursor("msg_0000", 1_700_000_000_000);
    let (s, body) = get(&app, &format!("/api/session/ses_p/message?cursor={v1}")).await;
    assert_eq!(
        s,
        StatusCode::BAD_REQUEST,
        "v1 cursor rejected on a v2 route"
    );
    assert_eq!(body["_tag"], "InvalidCursorError", "{body}");
}

#[tokio::test]
async fn unknown_session_is_tagged_404() {
    let (_st, app) = state_v2();
    let (s, b) = get(&app, "/api/session/ses_nope/message").await;
    assert_eq!(s, StatusCode::NOT_FOUND, "{b}");
    assert_eq!(b["_tag"], "SessionNotFoundError", "{b}");
}

// ---------------------------------------------------------------------------
// Session-list filters the shipped v2 clients actually send
// ---------------------------------------------------------------------------

#[tokio::test]
async fn session_list_honours_parent_null_and_archived() {
    let (st, app) = state_v2();
    seed_session(&st, "ses_root");
    seed_session(&st, "ses_child");
    // a subagent child: parentID set
    st.writer
        .write(vec![ocserve_store::WriteOp::Sql {
            sql: "UPDATE session SET parent_id = ?2 WHERE id = ?1".into(),
            params: vec!["ses_child".into(), "ses_root".into()],
        }])
        .unwrap();

    // The v2 web UI and TUI send exactly this to list root sessions.
    let (s, b) = get(
        &app,
        "/api/session?parentID=null&search=&archived=false&limit=50",
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(
        ids_of(&b),
        ["ses_root"],
        "parentID=null means root sessions only (the shipped clients rely on it)"
    );

    // A real id still filters to that parent's children.
    let (s, b) = get(&app, "/api/session?limit=50&parentID=ses_root").await;
    assert_eq!(s, StatusCode::OK, "{b}");
    assert_eq!(ids_of(&b), ["ses_child"], "{b}");
}

#[tokio::test]
async fn archived_is_a_real_state_not_a_filter_on_nothing() {
    let (st, app) = state_v2();
    seed_session(&st, "ses_live");
    seed_session(&st, "ses_old");
    let (s, _) = patch(
        &app,
        "/api/session/ses_old",
        &serde_json::json!({"archived": true}),
    )
    .await;
    assert_eq!(s, StatusCode::NO_CONTENT);

    let (_, live) = get(&app, "/api/session?limit=50&archived=false").await;
    assert_eq!(ids_of(&live), ["ses_live"], "archived excluded");
    let (_, old) = get(&app, "/api/session?limit=50&archived=true").await;
    assert_eq!(ids_of(&old), ["ses_old"], "archived included when asked");
    let (_, all) = get(&app, "/api/session?limit=50").await;
    assert_eq!(ids_of(&all).len(), 2, "omitting the filter includes both");

    // Restoring works, and re-asserting the same state must not reorder.
    let (s, _) = patch(
        &app,
        "/api/session/ses_old",
        &serde_json::json!({"archived": false}),
    )
    .await;
    assert_eq!(s, StatusCode::NO_CONTENT);
    let (_, live) = get(&app, "/api/session?limit=50&archived=false").await;
    assert_eq!(ids_of(&live).len(), 2, "restored session is live again");
}

#[tokio::test]
async fn session_list_cursor_advances_and_is_not_a_message_cursor() {
    let (st, app) = state_v2();
    for i in 0..5 {
        seed_session(&st, &format!("ses_{i}"));
        // distinct time_updated so the order is deterministic
        st.writer
            .write(vec![ocserve_store::WriteOp::Sql {
                sql: "UPDATE session SET time_updated = ?2 WHERE id = ?1".into(),
                params: vec![format!("ses_{i}").into(), (1000 + i).into()],
            }])
            .unwrap();
    }
    let (s, p1) = get(&app, "/api/session?limit=2").await;
    assert_eq!(s, StatusCode::OK, "{p1}");
    let c1 = p1["cursor"]["next"]
        .as_str()
        .expect("next cursor")
        .to_string();
    let (_, p2) = get(&app, &format!("/api/session?limit=2&cursor={c1}")).await;
    assert_ne!(ids_of(&p1), ids_of(&p2), "the cursor must advance");
    assert_ne!(
        p1["cursor"]["next"], p2["cursor"]["next"],
        "and the next cursor must move"
    );

    // A MESSAGE cursor on the session list is a different payload: rejected.
    let msg_cursor = ocserve_store::V2MessageCursor {
        id: "msg_x".into(),
        order: ocserve_store::MessageOrder::Desc,
        direction: ocserve_store::CursorDirection::Next,
    }
    .encode();
    let (s, b) = get(&app, &format!("/api/session?limit=2&cursor={msg_cursor}")).await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "{b}");
    assert_eq!(b["_tag"], "InvalidCursorError", "{b}");
}

fn ids_of(page: &Value) -> Vec<String> {
    page["data"]
        .as_array()
        .map(|a| {
            a.iter()
                .map(|s| s["id"].as_str().unwrap_or("").to_string())
                .collect()
        })
        .unwrap_or_default()
}

async fn patch(app: &axum::Router, uri: &str, body: &Value) -> (StatusCode, Value) {
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("PATCH")
                .uri(uri)
                .header("content-type", "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = resp.status();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

// ---------------------------------------------------------------------------
// VCS ref injection (SRE S1 / security VCS-1)
// ---------------------------------------------------------------------------

/// `?base=` arrives on an unauthenticated GET and used to flow straight into
/// `git diff <base>`. Git reads a leading `-` as an option, so
/// `base=--output=/tmp/x` wrote an attacker-chosen file and `base=--ext-diff`
/// re-enabled the repo's external diff driver after `--no-ext-diff`.
#[test]
fn vcs_ref_validation_rejects_options_and_bad_refs() {
    let dir = std::env::temp_dir().join(format!(
        "ocserve-vcsref-{}-{}",
        std::process::id(),
        next_fixture()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let run = |args: &[&str]| {
        std::process::Command::new("git")
            .args(args)
            .current_dir(&dir)
            .output()
            .unwrap()
    };
    run(&["init", "-q", "-b", "main"]);
    run(&["config", "user.email", "t@example.com"]);
    run(&["config", "user.name", "t"]);
    std::fs::write(dir.join("a.txt"), "one\n").unwrap();
    run(&["add", "."]);
    run(&["commit", "-qm", "one"]);

    let d = dir.to_str().unwrap();
    // An option-shaped value must never reach git as a flag.
    let pwn = dir.join("pwn.txt");
    let got = ocserve_http::vcs::diff_with(
        d,
        "git",
        None,
        Some(&format!("--output={}", pwn.display())),
        true,
    );
    assert!(
        got.as_array().map(|a| a.is_empty()).unwrap_or(false),
        "an option-shaped base must not produce a diff: {got}"
    );
    assert!(
        !pwn.exists(),
        "SECURITY: --output must not have written {:?}",
        pwn
    );

    // A ref that does not exist is empty, not an error and not a guess.
    let got = ocserve_http::vcs::diff_with(d, "git", None, Some("no-such-ref"), true);
    assert!(
        got.as_array().map(|a| a.is_empty()).unwrap_or(false),
        "{got}"
    );

    // A real ref still works (the feature is not broken by the validation).
    let got = ocserve_http::vcs::diff_with(d, "git", None, Some("HEAD"), true);
    assert!(got.is_array(), "{got}");
}
