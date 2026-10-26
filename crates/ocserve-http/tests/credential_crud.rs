//! Credential CRUD parity with the real 2.0.28 server (probed live
//! 2026-10-26): PATCH and DELETE were unchecked 204s that edited nothing, so a
//! client renaming a credential saw success and no change. Every status and
//! body below was captured from the reference before this test was written.
//!
//! Reference behaviour (probed, `bench/openapi/1.18.31.json` is the v1 wire;
//! these shapes are v2-only and live in the 2.0.28 contract):
//!   PATCH {label} -> 204, missing label -> 400 Payload
//!                    "Missing key\n  at [\"label\"]"
//!   PATCH {label: 5} -> 400 Payload "Expected string\n  at [\"label\"]"
//!   PATCH unknown id -> 204 (no-op)
//!   DELETE unknown id -> 204
//!   POST /api/integration/<known>/connect/key -> 204
//!   POST /api/integration/<unknown>/connect/key -> 404 IntegrationNotFoundError

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use tower::ServiceExt;

/// A minimal catalog with one known integration, so the connect endpoint's
/// known/unknown split is exercised against real data rather than an empty
/// fallback (an empty catalog would make every id unknown).
fn app() -> axum::Router {
    let p = ocserve_http::Payloads {
        catalog: std::sync::Arc::new(serde_json::json!({
            "tavily": {"name": "Tavily", "env": ["TAVILY_API_KEY"]}
        })),
        ..Default::default()
    };
    ocserve_http::router(ocserve_http::AppState::with_payloads_and_v2(None, p, true))
}

async fn req(
    app: &axum::Router,
    method: &str,
    uri: &str,
    body: Option<serde_json::Value>,
) -> (StatusCode, serde_json::Value) {
    let mut b = Request::builder().method(method).uri(uri);
    let body = match body {
        Some(v) => {
            b = b.header("content-type", "application/json");
            Body::from(serde_json::to_vec(&v).unwrap())
        }
        None => Body::empty(),
    };
    let resp = app.clone().oneshot(b.body(body).unwrap()).await.unwrap();
    let status = resp.status();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null),
    )
}

#[tokio::test]
async fn credential_create_then_rename_then_delete() {
    let app = app();
    let (st, created) = req(
        &app,
        "POST",
        "/api/credential",
        Some(serde_json::json!({
            "integrationID": "tavily",
            "value": {"type": "key", "key": "tvly-TEST"},
            "label": "mine",
            "activate": true,
        })),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "{created}");
    let id = created["data"]["id"].as_str().expect("id").to_string();
    assert!(id.starts_with("cred_"));
    assert_eq!(created["data"]["label"], "mine");

    // rename
    let (st, _) = req(
        &app,
        "PATCH",
        &format!("/api/credential/{id}"),
        Some(serde_json::json!({"label": "renamed"})),
    )
    .await;
    assert_eq!(st, StatusCode::NO_CONTENT);
    let (st, list) = req(&app, "GET", "/api/credential", None).await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(list["data"][0]["label"], "renamed");

    // delete
    let (st, _) = req(&app, "DELETE", &format!("/api/credential/{id}"), None).await;
    assert_eq!(st, StatusCode::NO_CONTENT);
    let (_, list) = req(&app, "GET", "/api/credential", None).await;
    assert_eq!(list["data"].as_array().map(|a| a.len()), Some(0));
}

#[tokio::test]
async fn credential_patch_errors_match_the_reference() {
    let app = app();
    let (st, body) = req(
        &app,
        "PATCH",
        "/api/credential/cred_doesnotexist",
        Some(serde_json::json!({})),
    )
    .await;
    assert_eq!(st, StatusCode::BAD_REQUEST);
    assert_eq!(body["_tag"], "InvalidRequestError");
    assert_eq!(body["message"], "Missing key\n  at [\"label\"]");
    assert_eq!(body["kind"], "Payload");

    let (st, body) = req(
        &app,
        "PATCH",
        "/api/credential/cred_doesnotexist",
        Some(serde_json::json!({"label": 5})),
    )
    .await;
    assert_eq!(st, StatusCode::BAD_REQUEST);
    assert_eq!(body["message"], "Expected string\n  at [\"label\"]");

    // Unknown ids are a 204 no-op on both verbs (probed).
    let (st, _) = req(
        &app,
        "PATCH",
        "/api/credential/cred_doesnotexist",
        Some(serde_json::json!({"label": "x"})),
    )
    .await;
    assert_eq!(st, StatusCode::NO_CONTENT);
    let (st, _) = req(&app, "DELETE", "/api/credential/cred_doesnotexist", None).await;
    assert_eq!(st, StatusCode::NO_CONTENT);
}

#[tokio::test]
async fn connect_key_splits_known_and_unknown_integrations() {
    let app = app();
    let (st, list) = req(&app, "GET", "/api/integration", None).await;
    assert_eq!(st, StatusCode::OK);
    assert_eq!(list["data"][0]["id"], "tavily");
    let known = "tavily".to_string();

    let (st, _) = req(
        &app,
        "POST",
        &format!("/api/integration/{known}/connect/key"),
        Some(serde_json::json!({"key": "x"})),
    )
    .await;
    assert_eq!(st, StatusCode::NO_CONTENT);

    let (st, body) = req(
        &app,
        "POST",
        "/api/integration/definitely-not-an-integration/connect/key",
        Some(serde_json::json!({"key": "x"})),
    )
    .await;
    assert_eq!(st, StatusCode::NOT_FOUND);
    assert_eq!(body["_tag"], "IntegrationNotFoundError");
    assert_eq!(body["integrationID"], "definitely-not-an-integration");
}
