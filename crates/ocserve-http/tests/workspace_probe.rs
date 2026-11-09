//! K-WORKSPACE — the v1 workspace family matches the reference, probed live.
//!
//! `POST /experimental/workspace` accepted `local` and `directory` as adapters
//! and minted a `wrk_` id for anything. The reference (1.19.5) has exactly one
//! adapter — `worktree` — refuses the rest with "Unknown workspace adapter",
//! and refuses a worktree on a non-git project with its own message. Handing
//! back an id for something that will never exist is shape without mutation.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use ocserve_http::AppState;
use serde_json::Value;
use tower::ServiceExt;

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
    let st = resp.status();
    let b = axum::body::to_bytes(resp.into_body(), 1 << 20)
        .await
        .unwrap();
    (st, serde_json::from_slice(&b).unwrap_or(Value::Null))
}

#[tokio::test]
async fn only_worktree_is_a_known_adapter() {
    // The AppState's directory is this checkout, which IS a git repo, so
    // `worktree` gets past the adapter check; `local`/`directory` must not.
    let app = ocserve_http::router(AppState::new());
    for bad in ["local", "directory", "bogus"] {
        let (st, v) = post(
            &app,
            "/experimental/workspace",
            serde_json::json!({"type": bad}),
        )
        .await;
        assert_eq!(st, StatusCode::BAD_REQUEST, "{bad} must be refused");
        assert_eq!(v["name"], "WorkspaceCreateError");
        assert_eq!(
            v["data"]["message"],
            format!("Unknown workspace adapter: {bad}"),
            "the reference's exact wording"
        );
    }
}
