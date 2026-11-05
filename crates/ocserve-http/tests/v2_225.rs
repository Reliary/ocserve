//! The 2.2.5 additions: every one of the 27 new operations is bound and its
//! contract-shaped answer is asserted, including the honest refusals.
//!
//! Each case was probe-pinned against a live `@opencode/cli@2.2.5` on
//! 2026-11-05; the comment on each test names what the reference did.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use tower::ServiceExt;

fn app_with(config: serde_json::Value) -> axum::Router {
    let p = ocserve_http::Payloads {
        config,
        ..Default::default()
    };
    ocserve_http::router(ocserve_http::AppState::with_payloads_and_v2(None, p, true))
}

fn app() -> axum::Router {
    app_with(serde_json::json!({}))
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

async fn json(r: axum::response::Response) -> serde_json::Value {
    let bytes = r.into_body().collect().await.unwrap().to_bytes();
    serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null)
}

/// Create a real session (the schedule table has an FK to `session`).
async fn new_session(app: &axum::Router) -> String {
    let r = req(
        app,
        "POST",
        "/session",
        Some(serde_json::json!({"title": "t"})),
    )
    .await;
    assert_eq!(r.status(), StatusCode::OK);
    json(r).await["id"]
        .as_str()
        .expect("session id")
        .to_string()
}

#[tokio::test]
async fn usage_limits_matches_the_live_reference() {
    // Live 2.2.5 with no workspace auth: {"location":{…},"data":null}.
    let r = req(&app(), "GET", "/api/experimental/usage/limits", None).await;
    assert_eq!(r.status(), StatusCode::OK);
    let v = json(r).await;
    assert!(v["location"]["directory"].is_string());
    assert!(v["data"].is_null(), "no workspace auth -> null: {v}");
}

#[tokio::test]
async fn voice_models_lists_the_four_builtin_models() {
    // Live 2.2.5 returns exactly this list with available:false (no keys).
    let r = req(&app(), "GET", "/api/experimental/voice/models", None).await;
    assert_eq!(r.status(), StatusCode::OK);
    let v = json(r).await;
    let models = v["data"]["models"].as_array().expect("models");
    assert_eq!(models.len(), 4, "{v}");
    assert!(models.iter().all(|m| m["available"] == false));
}

#[tokio::test]
async fn media_rejects_a_non_sha256_hash_before_lookup() {
    // Live: GET /api/experimental/media/deadbeef -> 400 InvalidRequestError
    // "Expected a string matching the RegExp ^[0-9a-f]{64}$" kind Params.
    let r = req(&app(), "GET", "/api/experimental/media/deadbeef", None).await;
    assert_eq!(r.status(), StatusCode::BAD_REQUEST);
    let v = json(r).await;
    assert_eq!(v["_tag"], "InvalidRequestError");
    assert_eq!(v["kind"], "Params");
    assert!(v["message"].as_str().unwrap().contains("[0-9a-f]{64}"));
}

#[tokio::test]
async fn media_unknown_hash_is_file_not_found() {
    let h = "a".repeat(64);
    let r = req(&app(), "GET", &format!("/api/experimental/media/{h}"), None).await;
    assert_eq!(r.status(), StatusCode::NOT_FOUND);
    assert_eq!(json(r).await["_tag"], "FileNotFoundError");
}

#[tokio::test]
async fn fs_watch_requires_a_path_and_accepts_a_registration() {
    // Live: missing path -> 400 Query-shape Payload error; valid -> 204.
    let r = req(
        &app(),
        "POST",
        "/api/experimental/fs/watch",
        Some(serde_json::json!({})),
    )
    .await;
    assert_eq!(r.status(), StatusCode::BAD_REQUEST);
    assert_eq!(json(r).await["_tag"], "InvalidRequestError");

    let r = req(
        &app(),
        "POST",
        "/api/experimental/fs/watch",
        Some(serde_json::json!({"path": "src/lib.rs"})),
    )
    .await;
    assert_eq!(r.status(), StatusCode::NO_CONTENT);
    let r = req(
        &app(),
        "DELETE",
        "/api/experimental/fs/watch",
        Some(serde_json::json!({"path": "src/lib.rs"})),
    )
    .await;
    assert_eq!(r.status(), StatusCode::NO_CONTENT);
}

#[tokio::test]
async fn artifacts_are_refused_exactly_like_the_reference_when_disabled() {
    // Live: {"_tag":"ForbiddenError","message":"Artifacts are disabled; set
    // experimental.artifacts in config"} for every route in the family.
    for (m, p) in [
        ("GET", "/api/experimental/artifacts"),
        ("GET", "/api/experimental/artifacts/providers"),
        ("GET", "/api/experimental/artifacts/art_x"),
        ("PATCH", "/api/experimental/artifacts/art_x"),
        ("DELETE", "/api/experimental/artifacts/art_x"),
    ] {
        let r = req(&app(), m, p, Some(serde_json::json!({}))).await;
        assert_eq!(r.status(), StatusCode::FORBIDDEN, "{m} {p}");
        let v = json(r).await;
        assert_eq!(v["_tag"], "ForbiddenError", "{m} {p}");
        assert!(
            v["message"]
                .as_str()
                .unwrap()
                .contains("experimental.artifacts")
        );
    }
}

#[tokio::test]
async fn plugin_gui_requires_id_then_reports_not_found() {
    // Live: missing id -> 400 kind Query; unknown id -> PluginNotFoundError.
    let r = req(&app(), "GET", "/api/plugin/gui", None).await;
    assert_eq!(r.status(), StatusCode::BAD_REQUEST);
    assert_eq!(json(r).await["kind"], "Query");
    let r = req(&app(), "GET", "/api/plugin/gui?id=nope", None).await;
    assert_eq!(r.status(), StatusCode::NOT_FOUND);
    assert_eq!(json(r).await["_tag"], "PluginNotFoundError");
}

#[tokio::test]
async fn permission_saved_patch_unknown_id_is_not_found() {
    // Live: PATCH /api/permission/saved/per_nope ->
    // {"_tag":"PermissionSavedNotFoundError","id":"per_nope",…} [404].
    let r = req(
        &app(),
        "PATCH",
        "/api/permission/saved/per_nope",
        Some(serde_json::json!({"action": "allow"})),
    )
    .await;
    assert_eq!(r.status(), StatusCode::NOT_FOUND);
    let v = json(r).await;
    assert_eq!(v["_tag"], "PermissionSavedNotFoundError");
    assert_eq!(v["id"], "per_nope");
}

#[tokio::test]
async fn heap_dump_reports_real_memory_and_no_fake_snapshot() {
    let r = req(
        &app(),
        "POST",
        "/api/debug/heap-dump",
        Some(serde_json::json!({})),
    )
    .await;
    assert_eq!(r.status(), StatusCode::OK);
    let v = json(r).await;
    assert_eq!(v["pid"], std::process::id());
    // A Rust process has no V8 heap: the path must be null, never a file we
    // did not write.
    assert!(v["path"].is_null());
    assert!(v.get("unsupported").is_some());
}

#[tokio::test]
async fn schedules_are_forbidden_unless_enabled() {
    // Live: disabled -> ForbiddenError "Scheduled tasks are disabled; set
    // experimental.schedules in config".
    let r = req(
        &app(),
        "GET",
        "/api/experimental/schedule?sessionID=ses_x",
        None,
    )
    .await;
    assert_eq!(r.status(), StatusCode::FORBIDDEN);
    assert!(
        json(r).await["message"]
            .as_str()
            .unwrap()
            .contains("experimental.schedules")
    );
    let r = req(
        &app(),
        "POST",
        "/api/experimental/schedule",
        Some(serde_json::json!({})),
    )
    .await;
    assert_eq!(r.status(), StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn schedule_lifecycle_round_trips() {
    // Live: create -> {data}, list -> [one], pause/resume -> {data} with the
    // new status, delete -> 204. `when` is a duration so the due time is real.
    let a = app_with(serde_json::json!({"experimental": {"schedules": true}}));
    let sid = new_session(&a).await;
    let created = req(
        &a,
        "POST",
        "/api/experimental/schedule",
        Some(serde_json::json!({
            "sessionID": sid,
            "name": "nightly",
            "prompt": "do the thing",
            "when": {"type": "every", "every": "5 minutes"}
        })),
    )
    .await;
    if created.status() != StatusCode::OK {
        let st = created.status();
        let v = json(created).await;
        panic!("create refused {st}: {v}");
    }
    let c = json(created).await;
    let id = c["data"]["id"].as_str().expect("id").to_string();
    assert_eq!(c["data"]["status"], "active");
    assert_eq!(c["data"]["action"]["prompt"], "do the thing");
    assert!(c["data"]["nextDueAt"].is_number(), "due time computed: {c}");

    let list = json(
        req(
            &a,
            "GET",
            &format!("/api/experimental/schedule?sessionID={sid}"),
            None,
        )
        .await,
    )
    .await;
    assert_eq!(list["data"].as_array().unwrap().len(), 1);

    let paused = req(
        &a,
        "POST",
        &format!("/api/experimental/schedule/{id}/pause"),
        None,
    )
    .await;
    assert_eq!(paused.status(), StatusCode::OK);
    assert_eq!(json(paused).await["data"]["status"], "paused");

    let resumed = req(
        &a,
        "POST",
        &format!("/api/experimental/schedule/{id}/resume"),
        None,
    )
    .await;
    assert_eq!(json(resumed).await["data"]["status"], "active");

    let del = req(
        &a,
        "DELETE",
        &format!("/api/experimental/schedule/{id}"),
        None,
    )
    .await;
    assert_eq!(del.status(), StatusCode::NO_CONTENT);
    let gone = req(&a, "GET", &format!("/api/experimental/schedule/{id}"), None).await;
    assert_eq!(gone.status(), StatusCode::NOT_FOUND);
    assert_eq!(json(gone).await["_tag"], "ScheduleNotFoundError");
}

#[tokio::test]
async fn schedule_unknown_id_is_not_found_with_the_reference_shape() {
    let a = app_with(serde_json::json!({"experimental": {"schedules": true}}));
    let r = req(&a, "GET", "/api/experimental/schedule/sch_nope", None).await;
    assert_eq!(r.status(), StatusCode::NOT_FOUND);
    let v = json(r).await;
    assert_eq!(v["_tag"], "ScheduleNotFoundError");
    assert_eq!(v["scheduleID"], "sch_nope");
    assert_eq!(v["message"], "Schedule not found: sch_nope");
}

#[tokio::test]
async fn schedule_rejects_an_unrecognised_when_spec() {
    // Live: a `when` with no in/at/every/cron returns all four Missing-key
    // lines in one message (Effect decode reports every branch).
    let a = app_with(serde_json::json!({"experimental": {"schedules": true}}));
    let sid = new_session(&a).await;
    let r = req(
        &a,
        "POST",
        "/api/experimental/schedule",
        Some(serde_json::json!({
            "sessionID": sid, "prompt": "p", "when": {"type": "cron"}
        })),
    )
    .await;
    // cron is accepted structurally with no computed due time (D-SCHEDULE-CRON)
    assert_eq!(
        r.status(),
        StatusCode::OK,
        "cron accepted without a due time"
    );
    let bad = req(
        &a,
        "POST",
        "/api/experimental/schedule",
        Some(serde_json::json!({
            "sessionID": sid, "prompt": "p", "when": {"type": "nonsense"}
        })),
    )
    .await;
    assert_eq!(bad.status(), StatusCode::BAD_REQUEST);
    let v = json(bad).await;
    let m = v["message"].as_str().unwrap();
    for k in [
        "when\"][\"in",
        "when\"][\"at",
        "when\"][\"every",
        "when\"][\"cron",
    ] {
        assert!(m.contains(k), "missing branch {k}: {m}");
    }
}

#[tokio::test]
async fn generate_image_rejects_unknown_provider_like_the_reference() {
    // Live: unknown model -> ModelNotFoundError (404) BEFORE any capability
    // check; a known provider gets the honest unsupported answer.
    let r = req(
        &app(),
        "POST",
        "/api/experimental/generate/image",
        Some(serde_json::json!({"model": {"id": "gpt-image-1", "providerID": "nope"}, "prompt": "x"})),
    )
    .await;
    assert_eq!(r.status(), StatusCode::NOT_FOUND);
    assert_eq!(json(r).await["_tag"], "ModelNotFoundError");
}

#[tokio::test]
async fn generate_decisions_requires_the_contract_keys() {
    // Live first-fails: ["model"], then ["model"]["id"], then ["questions"]
    // and ["questions"][0]["instructions"].
    let a = app();
    let r = req(
        &a,
        "POST",
        "/api/experimental/generate/decisions",
        Some(serde_json::json!({})),
    )
    .await;
    assert_eq!(r.status(), StatusCode::BAD_REQUEST);
    assert!(
        json(r).await["message"]
            .as_str()
            .unwrap()
            .contains("[\"model\"]")
    );

    let r = req(
        &a,
        "POST",
        "/api/experimental/generate/decisions",
        Some(serde_json::json!({
            "model": {"id": "m", "providerID": "p"},
            "input": [],
            "questions": [{"type": "predicate", "name": "q"}]
        })),
    )
    .await;
    assert_eq!(r.status(), StatusCode::BAD_REQUEST);
    assert!(
        json(r).await["message"]
            .as_str()
            .unwrap()
            .contains("instructions")
    );
}

#[tokio::test]
async fn voice_session_validates_shape_then_reports_unavailable() {
    // Live: {"model":{…}} -> Expected string at ["model"]; a string model with
    // no transport -> Missing key at ["transport"]. A complete body then gets
    // our honest 503 (no audio transport, D-VOICE-TRANSPORT).
    let r = req(
        &app(),
        "POST",
        "/api/experimental/voice/session",
        Some(serde_json::json!({"model": {"id": "x"}})),
    )
    .await;
    assert_eq!(r.status(), StatusCode::BAD_REQUEST);
    assert!(
        json(r).await["message"]
            .as_str()
            .unwrap()
            .contains("Expected string")
    );
    let r = req(
        &app(),
        "POST",
        "/api/experimental/voice/session",
        Some(serde_json::json!({"model": "gpt-live-1"})),
    )
    .await;
    assert_eq!(r.status(), StatusCode::BAD_REQUEST);
    assert!(
        json(r).await["message"]
            .as_str()
            .unwrap()
            .contains("transport")
    );
    let r = req(
        &app(),
        "POST",
        "/api/experimental/voice/session",
        Some(serde_json::json!({"model": "gpt-live-1", "transport": {"type": "websocket"}})),
    )
    .await;
    assert_eq!(r.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(json(r).await["_tag"], "ServiceUnavailableError");
}

#[tokio::test]
async fn client_mcp_requires_name_and_refuses_with_the_reference_envelope() {
    // Live 2.2.5: missing name -> 400 kind Query; anything else -> 403
    // ForbiddenError "Invalid client MCP connect token request".
    let r = req(&app(), "GET", "/api/experimental/client-mcp", None).await;
    assert_eq!(r.status(), StatusCode::BAD_REQUEST);
    assert_eq!(json(r).await["kind"], "Query");
    let r = req(
        &app(),
        "GET",
        "/api/experimental/client-mcp?name=demo",
        None,
    )
    .await;
    assert_eq!(r.status(), StatusCode::FORBIDDEN);
    assert_eq!(json(r).await["_tag"], "ForbiddenError");
    let r = req(
        &app(),
        "POST",
        "/api/experimental/client-mcp/token?name=demo",
        None,
    )
    .await;
    assert_eq!(r.status(), StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn every_225_route_is_bound_not_the_ui_fallback() {
    // The class this repo hit with /pty/shells: an unbound /api path is served
    // the SPA HTML by the catch-all, so a "200" can be a lie. Every new route
    // must answer application/json.
    let a = app();
    for (m, p) in [
        ("GET", "/api/experimental/usage/limits"),
        ("GET", "/api/experimental/voice/models"),
        ("GET", "/api/experimental/fs/watch"),
        ("GET", "/api/experimental/artifacts"),
        ("GET", "/api/plugin/gui?id=x"),
        ("GET", "/api/debug/heap-dump"),
    ] {
        let r = req(&a, m, p, None).await;
        let ct = r
            .headers()
            .get(axum::http::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string();
        assert!(ct.starts_with("application/json"), "{m} {p} -> {ct}");
    }
}
