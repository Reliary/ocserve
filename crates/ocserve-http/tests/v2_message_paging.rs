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

    let (s1, p1) = get(&app, "/api/session/ses_p/message?limit=5").await;
    assert_eq!(s1, StatusCode::OK, "{p1}");
    assert_eq!(
        ids(&p1),
        ["msg_0007", "msg_0008", "msg_0009", "msg_0010", "msg_0011"],
        "first page is the newest window, ASC"
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
        ["msg_0002", "msg_0003", "msg_0004", "msg_0005", "msg_0006"],
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
        ["msg_0000", "msg_0001"],
        "last page has the remaining two"
    );
    assert_eq!(
        next_cursor(&p3),
        None,
        "the walk terminates instead of looping forever"
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
async fn invalid_cursor_is_a_typed_400() {
    let (st, app) = state_v2();
    seed_session(&st, "ses_p");
    seed_msgs(&st, "ses_p", 1);

    let (s, body) = get(&app, "/api/session/ses_p/message?cursor=garbage").await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    assert_eq!(body["_tag"], "InvalidCursorError", "{body}");
}
