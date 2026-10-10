//! 2026-10-11 deep bug-hunt regressions: three confirmed wire/security
//! divergences found by the parallel discovery pass.
//!
//! 1. `GET /api/fs/list?path=../../etc` listed /etc (no containment) while
//!    freeze 500s — an arbitrary-directory disclosure. Now canonicalized and
//!    contained to the project root, exactly like `/file/content` and
//!    `/api/fs/read`.
//! 2. `POST /permission/{id}/reply` with a malformed body was silently coerced
//!    to `"reject"` (`unwrap_or("reject")`) and returned 404 for unknown ids —
//!    silently rejecting a real pending ask. Now the body is decoded first
//!    with the freeze Payload envelope (`Missing key at [reply]` /
//!    `Expected "once" | "always" | "reject", got …`), and an unknown id yields
//!    the tagged `PermissionNotFoundError`.
//! 3. `/event` served the /global/event `{payload}` envelope and `/api/event`
//!    the same — freeze serves bare `{id,type,properties}` on /event and v2
//!    `{id,type,data}` on /api/event (`Event` vs `V2Event` in the spec).
use axum::body::Body;
use axum::http::{Request, StatusCode};
use ocserve_http::AppState;
use serde_json::Value;
use tower::ServiceExt;

fn app() -> axum::Router {
    ocserve_http::router(AppState::new())
}

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
    let bytes = axum::body::to_bytes(resp.into_body(), 1 << 20)
        .await
        .unwrap();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

async fn get_status(app: &axum::Router, uri: &str) -> StatusCode {
    app.clone()
        .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
        .await
        .unwrap()
        .status()
}

// ---- 1. fs/list containment ------------------------------------------------

#[tokio::test]
async fn fs_list_rejects_traversal() {
    let app = app();
    // escape the (test cwd) project root; must be rejected, not listed
    assert_eq!(
        get_status(&app, "/api/fs/list?path=../../../../etc").await,
        StatusCode::BAD_REQUEST,
        "`..` escape must be rejected, not listed"
    );
    // in-root listing still works
    assert_eq!(
        get_status(&app, "/api/fs/list?path=.").await,
        StatusCode::OK
    );
}

// ---- 2. permission reply decode ---------------------------------------------

#[tokio::test]
async fn permission_reply_missing_key_is_400_payload() {
    let app = app();
    let (st, body) = post(
        &app,
        "/permission/per_bogus123456789012345678/reply",
        serde_json::json!({}),
    )
    .await;
    assert_eq!(
        st,
        StatusCode::BAD_REQUEST,
        "missing key decodes before 404"
    );
    assert_eq!(body["name"], "BadRequest");
    assert_eq!(body["data"]["kind"], "Payload");
    assert_eq!(body["data"]["message"], "Missing key\n  at [\"reply\"]");
}

#[tokio::test]
async fn permission_reply_bad_enum_is_400_then_tagged_404() {
    let app = app();
    let (st, body) = post(
        &app,
        "/permission/per_bogus123456789012345678/reply",
        serde_json::json!({"reply": 123}),
    )
    .await;
    assert_eq!(st, StatusCode::BAD_REQUEST);
    assert_eq!(
        body["data"]["message"],
        "Expected \"once\" | \"always\" | \"reject\", got 123\n  at [\"reply\"]"
    );
    // a valid enum for an unknown id → tagged not-found (not a silent reject)
    let (st2, body2) = post(
        &app,
        "/permission/per_bogus123456789012345678/reply",
        serde_json::json!({"reply": "once"}),
    )
    .await;
    assert_eq!(st2, StatusCode::NOT_FOUND);
    assert_eq!(body2["_tag"], "PermissionNotFoundError");
}

// ---- 3. /event and /api/event envelopes -------------------------------------

/// Read just the first `data:` line of an SSE response, bounded.
async fn first_sse_data(app: &axum::Router, uri: &str) -> Value {
    use futures_util::StreamExt as _;
    let resp = app
        .clone()
        .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
        .await
        .unwrap();
    let mut stream = resp.into_body().into_data_stream();
    let mut buf: Vec<u8> = Vec::new();
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(2);
    while buf.len() < 4096 {
        let chunk = tokio::time::timeout_at(deadline, stream.next()).await;
        match chunk {
            Ok(Some(Ok(c))) => {
                buf.extend_from_slice(&c);
                if buf.windows(2).any(|w| w == b"\n\n") {
                    break;
                }
            }
            _ => break,
        }
    }
    let text = String::from_utf8_lossy(&buf);
    let data = text
        .lines()
        .find_map(|l| l.strip_prefix("data: "))
        .unwrap_or("");
    serde_json::from_str(data).unwrap_or(Value::Null)
}

#[tokio::test]
async fn event_streams_use_their_frozen_envelopes() {
    let app = app();

    let global = first_sse_data(&app, "/global/event").await;
    assert!(
        global.get("payload").is_some(),
        "/global/event wraps payload: {global}"
    );
    assert_eq!(global["payload"]["type"], "server.connected");

    let bare = first_sse_data(&app, "/event").await;
    assert_eq!(bare["type"], "server.connected");
    assert!(
        bare.get("properties").is_some(),
        "/event is bare Event: {bare}"
    );
    assert!(
        bare.get("payload").is_none(),
        "/event must NOT wrap payload: {bare}"
    );

    let v2 = first_sse_data(&app, "/api/event").await;
    assert_eq!(v2["type"], "server.connected");
    assert!(
        v2.get("data").is_some(),
        "/api/event is V2Event {{id,type,data}}: {v2}"
    );
    assert!(
        v2.get("payload").is_none(),
        "/api/event must NOT wrap payload: {v2}"
    );
}

// ---- 4. session-scoped reads 404 on unknown sessions ------------------------

#[tokio::test]
async fn session_scoped_reads_404_unknown_session() {
    let app = app();
    let bogus = "ses_001a125ffffe00000000000000";
    // freeze 404s todo/children (ocserve returned []); diff is 200 [] on freeze
    for p in [
        format!("/session/{bogus}/todo"),
        format!("/session/{bogus}/children"),
    ] {
        assert_eq!(get_status(&app, &p).await, StatusCode::NOT_FOUND, "{p}");
    }
    // v2 session reads use the tagged SessionNotFoundError envelope (not v1)
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .uri(format!("/api/session/{bogus}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    let b: Value = serde_json::from_slice(
        &axum::body::to_bytes(resp.into_body(), 1 << 20)
            .await
            .unwrap(),
    )
    .unwrap();
    assert_eq!(b["_tag"], "SessionNotFoundError");
    assert_eq!(b["sessionID"], bogus);
}

// ---- 5. question routes: prefix decode + tagged not-found -------------------

#[tokio::test]
async fn question_routes_prefix_and_tagged_not_found() {
    let app = app();
    // bad prefix → 400 Params envelope (freeze decode precedes the 404)
    let (st, b) = post(&app, "/question/nope/reject", serde_json::json!({})).await;
    assert_eq!(st, StatusCode::BAD_REQUEST);
    assert_eq!(b["data"]["kind"], "Params");
    assert_eq!(
        b["data"]["message"],
        "Expected a string starting with \"que\", got \"nope\"\n  at [\"requestID\"]"
    );
    // valid prefix, unknown id → tagged QuestionNotFoundError
    let (st2, b2) = post(
        &app,
        "/question/que_bogus12345678901234567/reject",
        serde_json::json!({}),
    )
    .await;
    assert_eq!(st2, StatusCode::NOT_FOUND);
    assert_eq!(b2["_tag"], "QuestionNotFoundError");
    // reply variant
    let (st3, b3) = post(
        &app,
        "/question/nope/reply",
        serde_json::json!({"answers":[["x"]]}),
    )
    .await;
    assert_eq!(st3, StatusCode::BAD_REQUEST);
    assert_eq!(b3["data"]["kind"], "Params");
}
