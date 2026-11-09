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

/// The contract's `Workspace` requires id, type, name, projectID, timeUsed and
/// forbids extra keys. The first fix answered `{id,type}` — contract-invalid,
/// and invisible because spec-validate only walked GETs (it walks POSTs now).
#[tokio::test]
async fn a_created_workspace_satisfies_the_contract() {
    let app = ocserve_http::router(AppState::with_payloads_and_v2(
        None,
        ocserve_http::Payloads::default(),
        true,
    ));
    let (st, v) = post(
        &app,
        "/experimental/workspace",
        serde_json::json!({"type": "worktree"}),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "repo checkout is a git project: {v}");
    let obj = v.as_object().expect("object");
    let mut keys: Vec<&str> = obj.keys().map(|k| k.as_str()).collect();
    keys.sort_unstable();
    assert_eq!(
        keys,
        [
            "branch",
            "directory",
            "extra",
            "id",
            "name",
            "projectID",
            "timeUsed",
            "type"
        ],
        "exactly the contract's Workspace members"
    );
    assert!(v["id"].as_str().unwrap().starts_with("wrk"));
    assert!(!v["name"].as_str().unwrap().is_empty(), "name is required");
    // Nothing was created, so no directory is claimed: a path that does not
    // exist is the fabricated-answer class this route was fixed for.
    assert!(v["directory"].is_null(), "must not invent a path: {v}");
    // No directory key may point at something that exists on disk, because
    // this route creates nothing.
    assert!(
        v["directory"].is_null(),
        "create must not claim a directory it did not make"
    );
    assert!(!v["projectID"].as_str().unwrap().is_empty());
    assert!(v["timeUsed"].as_i64().unwrap() > 0);
}

#[tokio::test]
async fn deleting_a_workspace_decodes_the_id_brand_first() {
    let app = ocserve_http::router(AppState::with_payloads_and_v2(
        None,
        ocserve_http::Payloads::default(),
        true,
    ));
    // Probed 1.19.5: an unbranded id is a 400 Params error, not a 204.
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("DELETE")
                .uri("/experimental/workspace/nope")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let b = axum::body::to_bytes(resp.into_body(), 1 << 20)
        .await
        .unwrap();
    let v: Value = serde_json::from_slice(&b).unwrap();
    assert_eq!(v["data"]["kind"], "Params");
    assert!(
        v["data"]["message"]
            .as_str()
            .unwrap()
            .contains("starting with \"wrk\"")
    );
}
