//! Field-contract HTTP tests — the 2026-10-09 class closure (FIELD-CONTRACT.md).
//!
//! The double-prompt bug: readers used `messageId` while the frozen v1 wire
//! sends `messageID`, so client ids were silently dropped. These tests pin
//! THREE layers end-to-end:
//!   1. decode-error ENVELOPE bytes at the HTTP boundary (wire.rs unit tests
//!      pin the message strings; here we pin the route-family envelopes),
//!   2. decode precedes session-existence (probe: `{}` → 400, valid → 404),
//!   3. behavior: client id honored, noReply skips the model, command model
//!      string reaches endpoint resolution.
//!
//! All byte expectations are verbatim captures in
//! `bench/openapi/field-probes.md`.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use serde_json::json;
use tower::ServiceExt;

fn app() -> axum::Router {
    ocserve_http::router(ocserve_http::AppState::new())
}

/// Same router but with a dead endpoint wired for `fake/m` (golden's
/// prompt_async pattern) — `build_prompt_context` resolves BEFORE the
/// session check / noReply return, so endpoint-less states 400 first.
fn app_with_endpoint() -> axum::Router {
    use ocserve_http::{AppState, LlmRegistry, Payloads, Wires};
    let dir = std::env::temp_dir().join(format!(
        "ocserve-fc-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let db = ocserve_store::writer::db_path(&dir);
    let writer = std::sync::Arc::new(ocserve_store::Writer::spawn(db.clone()).unwrap());
    let blobs = std::sync::Arc::new(ocserve_store::BlobStore::new(dir.join("blobs")).unwrap());
    let llm = LlmRegistry {
        limits: std::collections::HashMap::new(),
        endpoints: [(
            "fake".to_string(),
            ("http://127.0.0.1:9".to_string(), String::new()),
        )]
        .into_iter()
        .collect(),
        pricing: Default::default(),
        default_model: ("fake".into(), "m".into()),
        systems: Default::default(),
        default_agent: "build".into(),
    };
    // a real command so /command reaches endpoint resolution (Payloads::default
    // carries no commands — template lookup would 400 first)
    let payloads = Payloads {
        command: vec![serde_json::json!({"name": "noop", "template": "run"})],
        ..Default::default()
    };
    let st = AppState::with_wiring(
        None,
        payloads,
        Wires {
            db,
            blobs,
            writer,
            llm,
        },
    );
    ocserve_http::router(st)
}

async fn body_json(resp: axum::response::Response) -> serde_json::Value {
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null)
}

async fn post(
    app: &axum::Router,
    uri: &str,
    body: serde_json::Value,
) -> (StatusCode, serde_json::Value) {
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(uri)
                .header("content-type", "application/json")
                .body(Body::from(serde_json::to_vec(&body).unwrap()))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = resp.status();
    (status, body_json(resp).await)
}

async fn new_session(app: &axum::Router) -> String {
    let (_, v) = post(app, "/session", json!({})).await;
    v["id"].as_str().expect("session id").to_string()
}

// ---------------------------------------------------------------------------
// v1 envelopes — `{"name":"BadRequest","data":{"message":…,"kind":"Payload"}}`
// ---------------------------------------------------------------------------

#[tokio::test]
async fn v1_message_missing_parts_exact_envelope() {
    let app = app();
    let sid = new_session(&app).await;
    let (st, v) = post(&app, &format!("/session/{sid}/message"), json!({})).await;
    assert_eq!(st, StatusCode::BAD_REQUEST);
    assert_eq!(
        v,
        json!({
            "name": "BadRequest",
            "data": {
                "message": "Missing key\n  at [\"parts\"]",
                "kind": "Payload"
            }
        })
    );
}

#[tokio::test]
async fn decode_precedes_session_existence() {
    // endpoint wired: otherwise build_prompt_context 400s before the 404
    let app = app_with_endpoint();
    // `{}` on a nonexistent session → 400 decode (probe-pinned), NOT 404
    let (st, v) = post(&app, "/session/ses_nonexistent_field/message", json!({})).await;
    assert_eq!(st, StatusCode::BAD_REQUEST, "decode precedes 404");
    assert_eq!(v["data"]["message"], "Missing key\n  at [\"parts\"]");
    // valid body on the same nonexistent session → 404
    let (st, v) = post(
        &app,
        "/session/ses_nonexistent_field/message",
        json!({"parts": []}),
    )
    .await;
    assert_eq!(st, StatusCode::NOT_FOUND, "valid decode then 404");
    assert_eq!(v["name"], "NotFoundError");
}

#[tokio::test]
async fn v1_invalid_messageid_exact_bytes() {
    let app = app();
    let sid = new_session(&app).await;
    let (st, v) = post(
        &app,
        &format!("/session/{sid}/message"),
        json!({"messageID": "notamsg", "parts": []}),
    )
    .await;
    assert_eq!(st, StatusCode::BAD_REQUEST);
    assert_eq!(
        v,
        json!({
            "name": "BadRequest",
            "data": {
                "message": "Expected a string starting with \"msg\", got \"notamsg\"\n  at [\"messageID\"]",
                "kind": "Payload"
            }
        })
    );
}

#[tokio::test]
async fn root_body_not_object_exact() {
    let app = app();
    let sid = new_session(&app).await;
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/session/{sid}/message"))
                .header("content-type", "application/json")
                .body(Body::from("[1]"))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let v = body_json(resp).await;
    assert_eq!(
        v["data"]["message"], "Expected object, got [1]",
        "root error carries NO `at` path"
    );
    assert!(!v["data"]["message"].as_str().unwrap().contains(" at "));
}

#[tokio::test]
async fn command_missing_arguments_exact_envelope() {
    let app = app();
    let sid = new_session(&app).await;
    let (st, v) = post(&app, &format!("/session/{sid}/command"), json!({})).await;
    assert_eq!(st, StatusCode::BAD_REQUEST);
    assert_eq!(
        v,
        json!({
            "name": "BadRequest",
            "data": {
                "message": "Missing key\n  at [\"arguments\"]",
                "kind": "Payload"
            }
        })
    );
}

#[tokio::test]
async fn shell_missing_agent_exact_envelope() {
    let app = app();
    let sid = new_session(&app).await;
    let (st, v) = post(
        &app,
        &format!("/session/{sid}/shell"),
        json!({"command": "ls"}),
    )
    .await;
    assert_eq!(st, StatusCode::BAD_REQUEST);
    // freeze bytes — the old ocserve-only messages ("command required",
    // "agent required") never existed upstream
    assert_eq!(
        v,
        json!({
            "name": "BadRequest",
            "data": {
                "message": "Missing key\n  at [\"agent\"]",
                "kind": "Payload"
            }
        })
    );
}

// ---------------------------------------------------------------------------
// v2 envelope — `{"_tag":"InvalidRequestError","message":…,"kind":"Payload"}`
// ---------------------------------------------------------------------------

#[tokio::test]
async fn v2_prompt_missing_prompt_exact_envelope() {
    let app = app();
    let sid = new_session(&app).await;
    let (st, v) = post(&app, &format!("/api/session/{sid}/prompt"), json!({})).await;
    assert_eq!(st, StatusCode::BAD_REQUEST);
    assert_eq!(
        v,
        json!({
            "_tag": "InvalidRequestError",
            "message": "Missing key\n  at [\"prompt\"]",
            "kind": "Payload"
        })
    );
}

#[tokio::test]
async fn v2_prompt_bad_id_uses_msg_underscore() {
    let app = app();
    let sid = new_session(&app).await;
    let (st, v) = post(
        &app,
        &format!("/api/session/{sid}/prompt"),
        json!({"id": "msgx", "prompt": {"text": "x"}}),
    )
    .await;
    assert_eq!(st, StatusCode::BAD_REQUEST);
    assert_eq!(
        v["message"],
        "Expected a string starting with \"msg_\", got \"msgx\"\n  at [\"id\"]"
    );
}

#[tokio::test]
async fn v2_permission_missing_action_exact_envelope() {
    let app = app();
    let sid = new_session(&app).await;
    let (st, v) = post(&app, &format!("/api/session/{sid}/permission"), json!({})).await;
    assert_eq!(st, StatusCode::BAD_REQUEST);
    assert_eq!(
        v,
        json!({
            "_tag": "InvalidRequestError",
            "message": "Missing key\n  at [\"action\"]",
            "kind": "Payload"
        })
    );
}

#[tokio::test]
async fn v2_permission_save_exact_envelope() {
    let app = app();
    let sid = new_session(&app).await;
    let (st, v) = post(
        &app,
        &format!("/api/session/{sid}/permission"),
        json!({"action": "bash", "resources": ["x"], "save": false}),
    )
    .await;
    assert_eq!(st, StatusCode::BAD_REQUEST);
    assert_eq!(
        v,
        json!({
            "_tag": "InvalidRequestError",
            "message": "Expected array, got false\n  at [\"save\"]",
            "kind": "Payload"
        })
    );
}

// ---------------------------------------------------------------------------
// behavior — client identity + noReply + command model string
// ---------------------------------------------------------------------------

#[tokio::test]
async fn no_reply_returns_user_message_without_model() {
    let app = app_with_endpoint();
    let sid = new_session(&app).await;
    // noReply must 200 with the USER message and never reach generation —
    // if it did, this AppState (no usable endpoint) would return an error.
    let (st, v) = post(
        &app,
        &format!("/session/{sid}/message"),
        json!({
            "messageID": "msg_noreply000000000000000001",
            "noReply": true,
            "parts": [{"type": "text", "text": "hi"}]
        }),
    )
    .await;
    assert_eq!(
        st,
        StatusCode::OK,
        "noReply returns 200 without a model call"
    );
    assert_eq!(v["info"]["role"], "user");
    assert_eq!(v["info"]["id"], "msg_noreply000000000000000001");
    assert_eq!(v["parts"].as_array().map(|a| a.len()), Some(1));
    // and the message is persisted with the CLIENT id
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .uri(format!("/session/{sid}/message"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let msgs = body_json(resp).await;
    let ids: Vec<&str> = msgs
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|m| m["info"]["id"].as_str())
        .collect();
    assert!(ids.contains(&"msg_noreply000000000000000001"));
}

#[tokio::test]
async fn lowercased_messageid_is_ignored_at_decode() {
    let app = app_with_endpoint();
    let sid = new_session(&app).await;
    // decode passes (unknown key ignored — onExcessProperty ignore) and the
    // server generates its OWN id: the supplied value must never be stored.
    let (st, _) = post(
        &app,
        &format!("/session/{sid}/message"),
        json!({
            "messageId": "msg_lc_neverstored0000000000001",
            "noReply": true,
            "parts": [{"type": "text", "text": "lc"}]
        }),
    )
    .await;
    assert_eq!(st, StatusCode::OK);
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .uri(format!("/session/{sid}/message"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let msgs = body_json(resp).await;
    let ids: Vec<&str> = msgs
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|m| m["info"]["id"].as_str())
        .collect();
    assert!(
        !ids.contains(&"msg_lc_neverstored0000000000001"),
        "lowercase messageId must be dropped — server generates the id"
    );
}

#[tokio::test]
async fn command_model_string_reaches_endpoint_resolution() {
    // endpoint wired for `fake` only: noprovider fails BY NAME, fake passes
    let app = app_with_endpoint();
    let sid = new_session(&app).await;
    // `model` on the command wire is a STRING; upstream parses it with
    // Provider.parseModel (first `/`). The previous Value-path read
    // `/model/providerID` off the string, silently used the session model,
    // and never surfaced the requested provider. A string now resolves —
    // proof: an unknown provider fails endpoint lookup by NAME.
    let (st, v) = post(
        &app,
        &format!("/session/{sid}/command"),
        json!({
            "command": "noop",
            "arguments": "",
            "model": "noprovider_xyz/whatever"
        }),
    )
    .await;
    assert_eq!(st, StatusCode::BAD_REQUEST);
    assert_eq!(
        v["data"]["message"],
        "no endpoint configured for provider noprovider_xyz"
    );
    // control: the same command with a RESOLVABLE provider string must get
    // past endpoint resolution (and fail later at generation instead)
    let (st2, v2) = post(
        &app,
        &format!("/session/{sid}/command"),
        json!({
            "command": "noop",
            "arguments": "",
            "model": "fake/m"
        }),
    )
    .await;
    assert_ne!(
        st2,
        StatusCode::BAD_REQUEST,
        "parseable provider string must pass endpoint resolution: {v2}"
    );
}

#[tokio::test]
async fn command_and_shell_and_prompt_family_decode_before_busy() {
    // decode runs before the per-session lock: a mid-prompt session must
    // still produce DECODE errors (not 409) for malformed bodies.
    let app = app();
    let sid = new_session(&app).await;
    let (st, v) = post(
        &app,
        &format!("/session/{sid}/command"),
        json!({"command": "init", "model": 123}),
    )
    .await;
    assert_eq!(st, StatusCode::BAD_REQUEST);
    assert_eq!(
        v["data"]["message"],
        "Expected string | null, got 123\n  at [\"model\"]"
    );
}

/// B4 runtime half: the four request structs' wire keys are pinned to the
/// frozen spec inside `ocserve-core::wire` (`wire_keys_match_frozen_spec`);
/// this asserts the decode path is actually WIRED for every prompt-family
/// route (a handler that skipped decode would turn these into 500s or 404s).
#[tokio::test]
async fn every_prompt_family_route_decodes() {
    let app = app();
    let sid = new_session(&app).await;
    for (uri, expect) in [
        (format!("/session/{sid}/message"), "parts"),
        (format!("/session/{sid}/prompt_async"), "parts"),
        (format!("/session/{sid}/command"), "arguments"),
        (format!("/session/{sid}/shell"), "agent"),
    ] {
        let (st, v) = post(&app, &uri, json!({})).await;
        assert_eq!(st, StatusCode::BAD_REQUEST, "{uri} must decode");
        assert!(
            v["data"]["message"]
                .as_str()
                .unwrap_or_default()
                .contains(expect),
            "{uri}: expected missing-key {expect}, got {v}"
        );
        assert_eq!(v["data"]["kind"], "Payload", "{uri} envelope kind");
    }
    let (st, v) = post(&app, &format!("/api/session/{sid}/prompt"), json!({})).await;
    assert_eq!(st, StatusCode::BAD_REQUEST);
    assert_eq!(v["_tag"], "InvalidRequestError");
}
