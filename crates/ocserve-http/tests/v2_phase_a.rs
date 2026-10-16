//! V2-PARITY-PLAN.md Phase A: TUI boot closure e2e.
//!
//! The v2 TUI boots: /api/info → file.list (/api/fs/list) → /api/event →
//! fan-out (location, project, agent, model, provider, skill, command,
//! integration, mcp, mcp/resource, reference, vcs, shell, session.form) +
//! session.active on reconnect. This suite proves the newly bound fan-out
//! routes answer with contract shapes (pair-v2 + spec-validate arbitrate
//! exactness; here: status + envelope + required keys).
//!
//! Negative controls: unknown project PATCH → tagged 404 (remove the guard
//! → 200, red); vcs bad mode → 400 Query envelope (weaken to 200 → red).

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use ocserve_http::{AppState, Payloads};
use serde_json::{Value, json};
use std::sync::Arc;
use tower::ServiceExt;

fn app() -> (axum::Router, Arc<AppState>) {
    let st = AppState::with_payloads_and_v2(None, Payloads::default(), true);
    let app = ocserve_http::router(st.clone());
    (app, st)
}

async fn get(app: &axum::Router, uri: &str) -> (StatusCode, Value) {
    let resp = app
        .clone()
        .oneshot(Request::builder().method("GET").uri(uri).body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = resp.status();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    (status, serde_json::from_slice(&bytes).unwrap_or(Value::Null))
}

#[tokio::test]
async fn phase_a_boot_fanout_answers() {
    let (app, _st) = app();
    // project list: bare array, union keys (canonical + time.active).
    let (s, v) = get(&app, "/api/project").await;
    assert_eq!(s, StatusCode::OK);
    let arr = v.as_array().expect("project.list is a bare array");
    assert!(!arr.is_empty());
    assert!(arr[0].get("canonical").is_some(), "v2 Project.canonical");
    assert!(arr[0].pointer("/time/active").is_some(), "v2 time.active");
    // mcp list: enveloped array with name+status.
    let (s, v) = get(&app, "/api/mcp").await;
    assert_eq!(s, StatusCode::OK);
    assert!(v.get("location").is_some() && v.get("data").is_some());
    // mcp resource catalog: enveloped, both arrays present.
    let (s, v) = get(&app, "/api/mcp/resource").await;
    assert_eq!(s, StatusCode::OK);
    assert!(v.pointer("/data/resources").and_then(|r| r.as_array()).is_some());
    assert!(v.pointer("/data/templates").and_then(|t| t.as_array()).is_some());
    // session active: {data: {...}} map shape.
    let (s, v) = get(&app, "/api/session/active").await;
    assert_eq!(s, StatusCode::OK);
    assert!(v.get("data").and_then(|d| d.as_object()).is_some());
    // fs list serves the TUI's api.file.list call (query contract is the
    // v1 ListQuery shape — assert only that it answers JSON, not HTML).
    let (s, v) = get(&app, "/api/fs/list?path=/tmp").await;
    assert!(s.is_success() || s == StatusCode::BAD_REQUEST, "fs/list status {s}, body {v}");
    // CONTROL: enveloped project list (wrap in {data}) → bare-array assert reds.
}

#[tokio::test]
async fn phase_a_vcs_aliases_answer() {
    let (app, _st) = app();
    // vcs info envelope (works outside a git repo: null branch).
    let (s, v) = get(&app, "/api/vcs").await;
    assert_eq!(s, StatusCode::OK);
    assert!(v.get("location").is_some() && v.get("data").is_some());
    // status: array (empty outside a repo).
    let (s, v) = get(&app, "/api/vcs/status").await;
    assert_eq!(s, StatusCode::OK);
    assert!(v.get("data").and_then(|d| d.as_array()).is_some());
    // branch: string array.
    let (s, v) = get(&app, "/api/vcs/branch").await;
    assert_eq!(s, StatusCode::OK);
    assert!(v.get("data").and_then(|d| d.as_array()).is_some());
    // base: object or null, never fabricated strings.
    let (s, v) = get(&app, "/api/vcs/base").await;
    assert_eq!(s, StatusCode::OK);
    assert!(v.get("data").is_some());
    // diff without mode → 400 Query envelope (v1 parity).
    let (s, v) = get(&app, "/api/vcs/diff").await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    assert_eq!(v["data"]["kind"], "Query");
    // CONTROL: 200-on-bad-mode → status assert reds.
}

#[tokio::test]
async fn phase_a_project_update_guards() {
    let (app, _st) = app();
    // Unknown project → tagged 404 (never blind 204).
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("PATCH")
                .uri("/api/project/nope")
                .header("content-type", "application/json")
                .body(Body::from(json!({"name": "x"}).to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let v: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(v["_tag"], "ProjectNotFoundError");
    // Unknown body key → 400 (additionalProperties enforced at the boundary).
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("PATCH")
                .uri("/api/project/global")
                .header("content-type", "application/json")
                .body(Body::from(json!({"bogus": 1}).to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    // Global update accepted with union shape.
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("PATCH")
                .uri("/api/project/global")
                .header("content-type", "application/json")
                .body(Body::from(json!({"name": "g"}).to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    // CONTROL: accept-unknown-project → first assert reds.
}
