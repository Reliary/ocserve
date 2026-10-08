//! Compat-batch route tests — freeze-probed shapes (2026-10-08):
//! GET /session/{id}/message/{mid}, POST /session/{id}/permissions/{pid},
//! POST /log. These close the web-UI SDK holes that previously fell through
//! to the UI proxy and crashed the app (the /pty/shells class).

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use tower::ServiceExt;

fn app() -> axum::Router {
    ocserve_http::router(ocserve_http::AppState::new())
}

async fn body_json(resp: axum::response::Response) -> serde_json::Value {
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null)
}

async fn req(
    app: &axum::Router,
    method: &str,
    uri: &str,
    body: Option<serde_json::Value>,
) -> axum::response::Response {
    let mut b = Request::builder().method(method).uri(uri);
    let body = match body {
        Some(v) => {
            b = b.header("content-type", "application/json");
            Body::from(serde_json::to_vec(&v).unwrap())
        }
        None => Body::empty(),
    };
    app.clone().oneshot(b.body(body).unwrap()).await.unwrap()
}

#[tokio::test]
async fn message_by_id_unknown_gets_notfound_envelope() {
    let app = app();
    // create a session so the route reaches the message lookup
    let resp = req(
        &app,
        "POST",
        "/session",
        Some(serde_json::json!({"title": "t"})),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let sid = body_json(resp).await["id"].as_str().unwrap().to_string();

    let mid = "msg_0fa1e3f770014jHIfQNh0Lhiu3";
    let resp = req(&app, "GET", &format!("/session/{sid}/message/{mid}"), None).await;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    let v = body_json(resp).await;
    // probed freeze: {"name":"NotFoundError","data":{"message":"Message not found: <mid>"}}
    assert_eq!(v["name"], "NotFoundError");
    assert_eq!(v["data"]["message"], format!("Message not found: {mid}"));
}

#[tokio::test]
async fn permission_respond_unknown_gets_tagged_envelope() {
    let app = app();
    let resp = req(
        &app,
        "POST",
        "/session",
        Some(serde_json::json!({"title": "t"})),
    )
    .await;
    let sid = body_json(resp).await["id"].as_str().unwrap().to_string();

    let pid = "per_0fa1e3f770014jHIfQNh0Lhiu3";
    let resp = req(
        &app,
        "POST",
        &format!("/session/{sid}/permissions/{pid}"),
        Some(serde_json::json!({"response": "once"})),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    let v = body_json(resp).await;
    // probed freeze: {"_tag":"PermissionNotFoundError","requestID","message"}
    assert_eq!(v["_tag"], "PermissionNotFoundError");
    assert_eq!(v["requestID"], pid);
    assert_eq!(v["message"], format!("Permission request not found: {pid}"));
}

#[tokio::test]
async fn log_route_returns_true() {
    let app = app();
    let resp = req(
        &app,
        "POST",
        "/log",
        Some(serde_json::json!({"service": "ui", "level": "error", "message": "boom"})),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(body_json(resp).await, serde_json::json!(true));
}

#[tokio::test]
async fn session_diff_is_not_a_route() {
    // POST /session/{id}/diff has no implementation (K-ADMIN divergence);
    // it falls through to the UI fallback exactly like any unknown path.
    // The GET variant IS a real freeze route returning [] — record that too.
    let app = app();
    let resp = req(
        &app,
        "POST",
        "/session",
        Some(serde_json::json!({"title": "t"})),
    )
    .await;
    let sid = body_json(resp).await["id"].as_str().unwrap().to_string();
    let resp = req(
        &app,
        "POST",
        &format!("/session/{sid}/diff"),
        Some(serde_json::json!({})),
    )
    .await;
    // no POST route → method-not-allowed fallback → UI proxy/404 (not a 405)
    assert_ne!(resp.status(), StatusCode::METHOD_NOT_ALLOWED);
}

#[tokio::test]
async fn revert_unrevert_marker_lifecycle() {
    let app = app();
    let resp = req(
        &app,
        "POST",
        "/session",
        Some(serde_json::json!({"title": "t"})),
    )
    .await;
    let sid = body_json(resp).await["id"].as_str().unwrap().to_string();

    // revert: missing messageID → 400 Payload envelope (freeze shape)
    let resp = req(
        &app,
        "POST",
        &format!("/session/{sid}/revert"),
        Some(serde_json::json!({})),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let v = body_json(resp).await;
    assert_eq!(v["data"]["kind"], "Payload");

    // revert with messageID → session Info carrying the marker
    let mid = "msg_0fa1e3f770014jHIfQNh0Lhiu3";
    let resp = req(
        &app,
        "POST",
        &format!("/session/{sid}/revert"),
        Some(serde_json::json!({"messageID": mid})),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let info = body_json(resp).await;
    assert_eq!(info["id"], sid);
    assert_eq!(info["revert"]["messageID"], mid);

    // persisted: GET reflects it
    let resp = req(&app, "GET", &format!("/session/{sid}"), None).await;
    let got = body_json(resp).await;
    assert_eq!(got["revert"]["messageID"], mid);

    // unrevert clears it
    let resp = req(&app, "POST", &format!("/session/{sid}/unrevert"), None).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let info = body_json(resp).await;
    assert!(
        info["revert"].is_null(),
        "revert must be null after unrevert: {info}"
    );
}

#[tokio::test]
async fn instance_dispose_returns_true() {
    let app = app();
    let resp = req(&app, "POST", "/instance/dispose", None).await;
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(body_json(resp).await, serde_json::json!(true));
}

#[tokio::test]
async fn oauth_and_mcp_auth_shapes() {
    let app = app();
    // provider oauth for a non-OAuth provider → 400 {"name":"BadRequest","data":{}}
    let resp = req(
        &app,
        "POST",
        "/provider/openai/oauth/authorize",
        Some(serde_json::json!({})),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let v = body_json(resp).await;
    assert_eq!(v["name"], "BadRequest");
    assert!(v["data"].as_object().map(|o| o.is_empty()).unwrap_or(false));

    // mcp auth authenticate for unknown server → 404 tagged
    let resp = req(&app, "POST", "/mcp/probe/auth/authenticate", None).await;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    let v = body_json(resp).await;
    assert_eq!(v["_tag"], "McpServerNotFoundError");
    assert_eq!(v["name"], "probe");
}

#[tokio::test]
async fn init_requires_model_fields() {
    let app = app();
    let resp = req(
        &app,
        "POST",
        "/session",
        Some(serde_json::json!({"title": "t"})),
    )
    .await;
    let sid = body_json(resp).await["id"].as_str().unwrap().to_string();
    let resp = req(
        &app,
        "POST",
        &format!("/session/{sid}/init"),
        Some(serde_json::json!({})),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let v = body_json(resp).await;
    assert_eq!(v["data"]["kind"], "Payload");
    // unknown session with full payload → 404
    let resp = req(
        &app,
        "POST",
        "/session/ses_missing/init",
        Some(serde_json::json!({"messageID": "msg_x", "providerID": "p", "modelID": "m"})),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}
