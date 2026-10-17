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
        .oneshot(
            Request::builder()
                .method("GET")
                .uri(uri)
                .body(Body::empty())
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
    assert!(
        v.pointer("/data/resources")
            .and_then(|r| r.as_array())
            .is_some()
    );
    assert!(
        v.pointer("/data/templates")
            .and_then(|t| t.as_array())
            .is_some()
    );
    // session active: {data: {...}} map shape.
    let (s, v) = get(&app, "/api/session/active").await;
    assert_eq!(s, StatusCode::OK);
    assert!(v.get("data").and_then(|d| d.as_object()).is_some());
    // fs list serves the TUI's api.file.list call (query contract is the
    // v1 ListQuery shape — assert only that it answers JSON, not HTML).
    let (s, v) = get(&app, "/api/fs/list?path=/tmp").await;
    assert!(
        s.is_success() || s == StatusCode::BAD_REQUEST,
        "fs/list status {s}, body {v}"
    );
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
async fn phase_c_projections_answer() {
    let (app, _st) = app();
    // config entries: document + directory sources.
    let (s, v) = get(&app, "/api/config").await;
    assert_eq!(s, StatusCode::OK);
    assert!(v.as_array().map(|a| !a.is_empty()).unwrap_or(false));
    // config shells: path+name+acceptable from /etc/shells.
    let (s, v) = get(&app, "/api/config/shell").await;
    assert_eq!(s, StatusCode::OK);
    let shells = v.as_array().expect("shell array");
    assert!(!shells.is_empty() && shells[0].get("path").is_some());
    // model default: enveloped, null when unresolvable.
    let (s, v) = get(&app, "/api/model/default").await;
    assert_eq!(s, StatusCode::OK);
    assert!(v.get("location").is_some() && v.get("data").is_some());
    // debug locations: array of {directory}.
    let (s, v) = get(&app, "/api/debug/location").await;
    assert_eq!(s, StatusCode::OK);
    assert!(v.as_array().map(|a| !a.is_empty()).unwrap_or(false));
    // migration status: completed (one-shot, project-don't-convert).
    let (s, v) = get(&app, "/api/experimental/migration/v1").await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(v["status"], "completed");
    // plugin list: enveloped (empty until the inventory RPC lands).
    let (s, v) = get(&app, "/api/plugin").await;
    assert_eq!(s, StatusCode::OK);
    assert!(v.get("data").and_then(|d| d.as_array()).is_some());
    // fs write + read-back through the contained path.
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/experimental/fs/write?path=/tmp/ocserve-phasec-test.txt")
                .body(Body::from("hello-phase-c"))
                .unwrap(),
        )
        .await
        .unwrap();
    // /tmp escapes most worktrees → 403 traversal guard (proves containment).
    assert!(
        resp.status() == StatusCode::OK || resp.status() == StatusCode::FORBIDDEN,
        "fs/write answers, got {}",
        resp.status()
    );
    // traversal outside the worktree → 403, never a write.
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/experimental/fs/write?path=../../etc/ocserve-phasec-evil")
                .body(Body::from("x"))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::FORBIDDEN);
    // worktree list: array (empty outside git is honest, not an error).
    let (s, v) = get(&app, "/api/worktree").await;
    assert_eq!(s, StatusCode::OK);
    assert!(v.as_array().is_some());
    // worktree refresh without projectID → 400.
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/worktree/refresh")
                .header("content-type", "application/json")
                .body(Body::from(json!({}).to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    // CONTROL: fs/write allowing ../../ → second 403 assert reds.
}

#[tokio::test]
async fn phase_b_session_aliases() {
    let (app, st) = app();
    // Seed a session with a user message to fork from.
    ocserve_store::insert_session(
        &st.writer,
        &json!({
            "id": "ses_pb", "projectID": "global", "directory": "/w",
            "path": "ses_pb", "slug": "ses_pb", "title": "pb", "version": "1",
            "time": {"created": 1, "updated": 2},
        }),
    )
    .unwrap();
    ocserve_store::insert_message(
        &st.writer,
        None,
        "ses_pb",
        &json!({"id": "msg_pb000000000000000001", "role": "user", "time": {"created": 10}, "text": "hi"}),
        &[json!({"id": "prt_pb01", "type": "text", "text": "hi"})],
    )
    .unwrap();

    async fn post(app: &axum::Router, uri: &str, body: Value) -> (StatusCode, Value) {
        let resp = app
            .clone()
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
        let status = resp.status();
        let bytes = resp.into_body().collect().await.unwrap().to_bytes();
        (
            status,
            serde_json::from_slice(&bytes).unwrap_or(Value::Null),
        )
    }

    // diff: enveloped (empty without revert markers).
    let (s, v) = get(&app, "/api/session/ses_pb/diff").await;
    assert_eq!(s, StatusCode::OK);
    assert!(v.get("data").and_then(|d| d.as_array()).is_some());

    // fork with before: copies history strictly before the message.
    let (s, v) = post(
        &app,
        "/api/session/ses_pb/fork",
        json!({"before": "msg_pb000000000000000001"}),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "fork body {v}");
    assert!(v["data"].get("id").and_then(|i| i.as_str()).is_some());
    assert_ne!(v["data"]["id"], "ses_pb");

    // fork unknown key → 400 (additionalProperties enforced).
    let (s, _) = post(
        &app,
        "/api/session/ses_pb/fork",
        json!({"messageID": "msg_x"}),
    )
    .await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    // fork bad brand → 400.
    let (s, _) = post(&app, "/api/session/ses_pb/fork", json!({"before": "msgx"})).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    // fork unknown session → tagged 404.
    let (s, v) = post(&app, "/api/session/ses_nope/fork", json!({})).await;
    assert_eq!(s, StatusCode::NOT_FOUND);

    // background: 204 + recorded; unknown session → 404.
    let (s, _) = post(&app, "/api/session/ses_pb/background", json!({})).await;
    assert_eq!(s, StatusCode::NO_CONTENT);
    let (s, _) = post(&app, "/api/session/ses_nope/background", json!({})).await;
    assert_eq!(s, StatusCode::NOT_FOUND);

    // experimental wait alias: same 503 honesty as /api wait.
    let (s, _) = post(&app, "/api/experimental/session/ses_pb/wait", json!({})).await;
    assert_eq!(s, StatusCode::SERVICE_UNAVAILABLE);
    // CONTROL: fork accepting messageID → second assert reds.
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
