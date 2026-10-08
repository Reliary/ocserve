//! `/tui/*` external-controller contract (PLAN §17; probed live against
//! freeze 1.18.31 on 2026-10-08).
//!
//! The corpus can't carry these (every response is `true`; the contract is
//! the *event published onto the SSE stream*), so this suite asserts both
//! sides: HTTP status/body shapes AND the exact event frames a subscriber
//! observes — including the upstream quirks (open-themes → session.list;
//! unknown execute-command → properties {} ; toast duration default 5000).
use axum::body::Body;
use axum::http::{Request, StatusCode};
use ocserve_http::{AppState, LlmRegistry, Payloads, Wires};
use serde_json::json;
use std::sync::Arc;
use tower::ServiceExt;

fn state() -> (Arc<AppState>, axum::Router) {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("t.db");
    let blobs = Arc::new(ocserve_store::BlobStore::new(dir.path().join("blobs")).unwrap());
    let writer = Arc::new(ocserve_store::Writer::spawn(db.clone()).unwrap());
    let llm = LlmRegistry {
        endpoints: Default::default(),
        pricing: Default::default(),
        limits: Default::default(),
        default_model: ("fake".into(), "m".into()),
        systems: Default::default(),
        default_agent: "build".into(),
    };
    let st = AppState::with_wiring(
        None,
        Payloads::default(),
        Wires {
            db,
            blobs,
            writer,
            llm,
        },
    );
    std::mem::forget(dir); // keep the tempdir alive for the test's duration
    let app = ocserve_http::router(st.clone());
    (st, app)
}

async fn post(app: &axum::Router, uri: &str, body: serde_json::Value) -> (StatusCode, Vec<u8>) {
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
        .unwrap()
        .to_vec();
    (status, bytes)
}

/// Collect `tui.*` events received by a bus subscriber after `f()` runs.
async fn tui_events(
    st: &Arc<AppState>,
    f: impl std::future::Future<Output = ()>,
) -> Vec<serde_json::Value> {
    let mut rx = st.bus.subscribe();
    f.await;
    let mut out = Vec::new();
    // Drain everything already queued (publish is synchronous; no sleep needed).
    while let Ok(frame) = rx.try_recv() {
        let v: serde_json::Value = serde_json::from_str(&frame).unwrap();
        let payload = &v["payload"];
        if payload["type"] == "sync" {
            continue;
        }
        if let Some(t) = payload["type"].as_str()
            && t.starts_with("tui.")
        {
            out.push(payload.clone());
        }
    }
    out
}

#[tokio::test]
async fn append_prompt_publishes_text_and_true() {
    let (st, app) = state();
    let events = tui_events(&st, async {
        let (s, b) = post(&app, "/tui/append-prompt", json!({"text": "HELLO"})).await;
        assert_eq!(s, StatusCode::OK);
        assert_eq!(b, b"true");
    })
    .await;
    assert_eq!(events.len(), 1);
    assert_eq!(events[0]["type"], "tui.prompt.append");
    assert_eq!(events[0]["properties"]["text"], "HELLO");
}

#[tokio::test]
async fn dialog_openers_publish_the_freeze_commands() {
    let (st, app) = state();
    let events = tui_events(&st, async {
        for path in [
            "/tui/open-help",
            "/tui/open-sessions",
            "/tui/open-themes",
            "/tui/open-models",
            "/tui/submit-prompt",
            "/tui/clear-prompt",
        ] {
            let (s, _) = post(&app, path, json!({})).await;
            assert_eq!(s, StatusCode::OK, "{path}");
        }
    })
    .await;
    let cmds: Vec<_> = events
        .iter()
        .map(|e| {
            e["properties"]["command"]
                .as_str()
                .unwrap_or("")
                .to_string()
        })
        .collect();
    assert_eq!(
        cmds,
        vec![
            "help.show",
            "session.list",
            "session.list", // open-themes quirk: upstream publishes session.list
            "model.list",
            "prompt.submit",
            "prompt.clear",
        ]
    );
}

#[tokio::test]
async fn execute_command_aliases_unknown_and_missing() {
    let (st, app) = state();
    let events = tui_events(&st, async {
        let (s, b) = post(
            &app,
            "/tui/execute-command",
            json!({"command": "session_new"}),
        )
        .await;
        assert_eq!(s, StatusCode::OK);
        assert_eq!(b, b"true");
    })
    .await;
    assert_eq!(events[0]["properties"]["command"], "session.new");

    // unknown alias → properties {} (not omitted event)
    let (st, app) = state();
    let events = tui_events(&st, async {
        let (s, _) = post(&app, "/tui/execute-command", json!({"command": "nope"})).await;
        assert_eq!(s, StatusCode::OK);
    })
    .await;
    assert_eq!(events.len(), 1);
    assert_eq!(events[0]["type"], "tui.command.execute");
    assert!(
        events[0]["properties"].get("command").is_none(),
        "unknown alias must publish empty properties: {events:?}"
    );

    // missing command → 400 payload envelope
    let (_, app) = state();
    let (s, b) = post(&app, "/tui/execute-command", json!({})).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    let v: serde_json::Value = serde_json::from_slice(&b).unwrap();
    assert_eq!(v["name"], "BadRequest");
    assert_eq!(v["data"]["kind"], "Payload");
}

#[tokio::test]
async fn toast_defaults_duration_and_requires_nothing() {
    let (st, app) = state();
    let events = tui_events(&st, async {
        let (s, _) = post(
            &app,
            "/tui/show-toast",
            json!({"message": "hi", "variant": "info"}),
        )
        .await;
        assert_eq!(s, StatusCode::OK);
    })
    .await;
    assert_eq!(events[0]["type"], "tui.toast.show");
    assert_eq!(
        events[0]["properties"]["duration"], 5000,
        "DEFAULT_TOAST_DURATION"
    );
    assert_eq!(events[0]["properties"]["message"], "hi");
}

#[tokio::test]
async fn publish_dispatch_known_types_only() {
    let (st, app) = state();
    let events = tui_events(&st, async {
        let (s, _) = post(
            &app,
            "/tui/publish",
            json!({"type": "tui.prompt.append", "properties": {"text": "PUB"}}),
        )
        .await;
        assert_eq!(s, StatusCode::OK);
        // unknown type: answered true, publishes nothing
        let (s, _) = post(
            &app,
            "/tui/publish",
            json!({"type": "tui.unknown.thing", "properties": {}}),
        )
        .await;
        assert_eq!(s, StatusCode::OK);
    })
    .await;
    assert_eq!(events.len(), 1, "only the known type publishes: {events:?}");
    assert_eq!(events[0]["properties"]["text"], "PUB");
}

#[tokio::test]
async fn select_session_validates_shape_then_existence() {
    let (st, app) = state();
    // non-ses → 400 with the exact upstream message (including the trailer)
    let (s, b) = post(&app, "/tui/select-session", json!({"sessionID": "bad"})).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    let v: serde_json::Value = serde_json::from_slice(&b).unwrap();
    assert_eq!(
        v["data"]["message"],
        "Expected a string starting with \"ses\", got \"bad\"\n  at [\"sessionID\"]"
    );
    // ses-shaped but absent → 404 envelope
    let (s, b) = post(
        &app,
        "/tui/select-session",
        json!({"sessionID": "ses_none"}),
    )
    .await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    let v: serde_json::Value = serde_json::from_slice(&b).unwrap();
    assert_eq!(v["name"], "NotFoundError");

    // existing → published select event
    ocserve_store::insert_session(
        &st.writer,
        &json!({
            "id": "ses_here", "projectID": "global", "directory": "/w",
            "path": "ses_here", "slug": "ses_here", "title": "t", "version": "1",
            "time": {"created": 1, "updated": 2},
        }),
    )
    .unwrap();
    let events = tui_events(&st, async {
        let (s, b) = post(
            &app,
            "/tui/select-session",
            json!({"sessionID": "ses_here"}),
        )
        .await;
        assert_eq!(s, StatusCode::OK);
        assert_eq!(b, b"true");
    })
    .await;
    assert_eq!(events[0]["type"], "tui.session.select");
    assert_eq!(events[0]["properties"]["sessionID"], "ses_here");
}

#[tokio::test]
async fn control_rendezvous_round_trips_and_next_returns_queued_request() {
    let (st, app) = state();
    // queue one request, then GET /tui/control/next must return it
    assert!(
        st.tui
            .submit_request(json!({"path": "/x", "body": {"k": 1}}))
            .await,
        "queue accepts while a consumer exists"
    );
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/tui/control/next")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let v: serde_json::Value = serde_json::from_slice(
        &axum::body::to_bytes(resp.into_body(), 1 << 20)
            .await
            .unwrap(),
    )
    .unwrap();
    assert_eq!(v["path"], "/x");

    // response side: POST answers true
    let (s, b) = post(&app, "/tui/control/response", json!({"ok": true})).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(b, b"true");
}
