//! K-SYNC-CONTRACT — the three `/sync/*` routes answer contract-shaped bodies.
//!
//! They answered bare literals (`true`) or the wrong envelope (`{data:[]}`),
//! which no generated client can decode. `pair-check`'s spec validation caught
//! all three on 2026-11-09; this locks the shapes without needing the reference.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use ocserve_http::AppState;
use serde_json::{Value, json};
use std::sync::Arc;
use tower::ServiceExt;

fn app() -> Arc<AppState> {
    AppState::with_payloads_and_v2(None, ocserve_http::Payloads::default(), true)
}

async fn post(app: &Arc<AppState>, uri: &str, body: Value) -> (StatusCode, Value) {
    let resp = ocserve_http::router(app.clone())
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(uri)
                .header("content-type", "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    let st = resp.status();
    let b = axum::body::to_bytes(resp.into_body(), 8 << 20)
        .await
        .unwrap();
    (st, serde_json::from_slice(&b).unwrap_or(Value::Null))
}

async fn session(app: &Arc<AppState>) -> String {
    let (st, v) = post(app, "/session", json!({"title": "sync"})).await;
    assert_eq!(st, StatusCode::OK);
    v["id"].as_str().unwrap().to_string()
}

#[tokio::test]
async fn steal_answers_the_session_id_not_a_bare_boolean() {
    let app = app();
    let sid = session(&app).await;
    let (st, v) = post(&app, "/sync/steal", json!({"sessionID": sid})).await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(v["sessionID"], sid.as_str(), "got {v}");

    let (st, v) = post(&app, "/sync/steal", json!({})).await;
    assert_eq!(st, StatusCode::BAD_REQUEST, "missing sessionID: {v}");
    // Probed 1.19.5: an unbranded id is a 400 too.
    let (st, _) = post(&app, "/sync/steal", json!({"sessionID": "nope"})).await;
    assert_eq!(st, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn replay_requires_directory_and_events_and_returns_the_session() {
    let app = app();
    let sid = session(&app).await;
    let ev = json!({"id": "evt_abc", "aggregateID": sid, "seq": 1,
                    "type": "session.updated", "data": {}});

    let (st, v) = post(&app, "/sync/replay", json!({"events": [ev]})).await;
    assert_eq!(st, StatusCode::BAD_REQUEST, "directory required: {v}");

    let (st, v) = post(&app, "/sync/replay", json!({"directory": "/tmp"})).await;
    assert_eq!(st, StatusCode::BAD_REQUEST, "events required: {v}");

    let (st, v) = post(
        &app,
        "/sync/replay",
        json!({"directory": "/tmp", "events": []}),
    )
    .await;
    assert_eq!(st, StatusCode::BAD_REQUEST, "minItems 1: {v}");

    let ev = json!({"id": "evt_abc", "aggregateID": sid, "seq": 1,
                    "type": "session.updated", "data": {}});
    let (st, v) = post(
        &app,
        "/sync/replay",
        json!({"directory": "/tmp", "events": [ev]}),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "valid replay: {v}");
    assert_eq!(v["sessionID"], sid.as_str());
}

#[tokio::test]
async fn history_is_an_array_and_honours_the_per_aggregate_watermark() {
    let app = app();
    let sid = session(&app).await;
    // Session create is live-only (the v1 contract publishes `session.created`
    // on the bus, not into the durable log), so seed the log the way a turn
    // does: two durable events for this aggregate.
    ocserve_store::emit_event(&app.writer, Some(&sid), "session.updated", &json!({"n": 1}))
        .unwrap();
    ocserve_store::emit_event(&app.writer, Some(&sid), "session.updated", &json!({"n": 2}))
        .unwrap();
    // An EMPTY body is deliberately `[]`: the reference materialises the whole
    // log for `{}`, which OOM-killed this server on 2026-11-09 (73k events,
    // 542 MB) — D-SYNC-HISTORY-BOUND. The real caller always names the
    // aggregates it wants.
    let (st, empty) = post(&app, "/sync/history", json!({})).await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(empty, json!([]), "empty body must not dump the log");

    let (st, all) = post(&app, "/sync/history", json!({sid.clone(): 0})).await;
    assert_eq!(st, StatusCode::OK);
    let arr = all.as_array().expect("an ARRAY, not {data:...}");
    assert!(!arr.is_empty(), "the session's events");
    let first = &arr[0];
    assert!(first["id"].as_str().unwrap().starts_with("evt_"));
    assert!(first["seq"].is_i64());
    assert!(first["aggregate_id"].is_string());
    assert!(first["data"].is_object());

    // A watermark at the newest seq for this aggregate returns nothing after
    // it (exclusive), which the first implementation got wrong by ignoring the
    // cursor entirely.
    let newest = arr
        .iter()
        .filter(|e| e["aggregate_id"] == sid.as_str())
        .map(|e| e["seq"].as_i64().unwrap())
        .max()
        .expect("this session has events");
    let (st, after) = post(&app, "/sync/history", json!({sid.clone(): newest})).await;
    assert_eq!(st, StatusCode::OK);
    assert!(
        after.as_array().unwrap().is_empty(),
        "nothing after the newest seq: {after}"
    );

    let (st, some) = post(&app, "/sync/history", json!({sid.clone(): 0})).await;
    assert_eq!(st, StatusCode::OK);
    assert!(
        !some.as_array().unwrap().is_empty(),
        "seq > 0 returns the session's events"
    );
}

/// The bound that matters: a request for ONE aggregate must not return another
/// aggregate's events, and must not walk the whole table. The pre-fix handler
/// loaded everything and died at the 768 MB cap.
#[tokio::test]
async fn history_returns_only_the_aggregates_the_caller_names() {
    let app = app();
    let a = session(&app).await;
    let b = session(&app).await;
    for i in 0..3 {
        ocserve_store::emit_event(&app.writer, Some(&a), "session.updated", &json!({"a": i}))
            .unwrap();
        ocserve_store::emit_event(&app.writer, Some(&b), "session.updated", &json!({"b": i}))
            .unwrap();
    }
    let (st, v) = post(&app, "/sync/history", json!({a.clone(): 0})).await;
    assert_eq!(st, StatusCode::OK);
    let arr = v.as_array().unwrap();
    assert_eq!(arr.len(), 3, "only session A's three events: {v}");
    assert!(arr.iter().all(|e| e["aggregate_id"] == a.as_str()));
}
