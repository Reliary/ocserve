//! K-V2-LAUNCHER: `GET /api/session?parentID=` must return CHILDREN.
//!
//! Paseo's v2 children reconciler calls `session.list({parentID})`. Before
//! this filter existed the route returned every session, so the parent was
//! treated as its own child and Paseo bound it twice ("OpenCode session … is
//! already bound to a Paseo agent") on the very first agent creation.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use ocserve_http::{AppState, Payloads};
use serde_json::{Value, json};
use tower::ServiceExt;

fn app() -> axum::Router {
    ocserve_http::router(AppState::with_payloads_and_v2(
        None,
        Payloads::default(),
        true,
    ))
}

async fn call(
    app: &axum::Router,
    method: &str,
    uri: &str,
    body: Option<Value>,
) -> (StatusCode, Value) {
    let mut b = Request::builder().method(method).uri(uri);
    if body.is_some() {
        b = b.header("content-type", "application/json");
    }
    let body = body.map(|v| Body::from(v.to_string())).unwrap_or_default();
    let resp = app.clone().oneshot(b.body(body).unwrap()).await.unwrap();
    let status = resp.status();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

#[tokio::test]
async fn parent_id_filter_excludes_the_parent_and_unrelated_sessions() {
    let app = app();
    let (s, v) = call(
        &app,
        "POST",
        "/api/session",
        Some(json!({"title": "parent"})),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{v}");
    let parent = v["data"]["id"].as_str().expect("parent id").to_string();
    let (s, v) = call(
        &app,
        "POST",
        "/api/session",
        Some(json!({"title": "child", "parentID": parent})),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "{v}");
    let child = v["data"]["id"].as_str().expect("child id").to_string();
    assert_eq!(v["data"]["parentID"], parent, "parent link is stored");

    let (s, v) = call(
        &app,
        "GET",
        &format!("/api/session?parentID={parent}"),
        None,
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    let ids: Vec<&str> = v["data"]
        .as_array()
        .expect("data array")
        .iter()
        .filter_map(|s| s["id"].as_str())
        .collect();
    assert_eq!(
        ids,
        vec![child.as_str()],
        "only the child belongs to {parent}: {v}"
    );

    // Negative control: the unfiltered list still contains both sessions.
    let (_, all) = call(&app, "GET", "/api/session", None).await;
    let all_ids: Vec<&str> = all["data"]
        .as_array()
        .expect("data array")
        .iter()
        .filter_map(|s| s["id"].as_str())
        .collect();
    assert!(all_ids.contains(&parent.as_str()));
    assert!(all_ids.contains(&child.as_str()));

    // roots=true returns the parent only.
    let (_, roots) = call(&app, "GET", "/api/session?roots=true", None).await;
    let root_ids: Vec<&str> = roots["data"]
        .as_array()
        .expect("data array")
        .iter()
        .filter_map(|s| s["id"].as_str())
        .collect();
    assert!(root_ids.contains(&parent.as_str()));
    assert!(!root_ids.contains(&child.as_str()));
}
