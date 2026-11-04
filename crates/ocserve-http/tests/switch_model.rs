//! K-SWITCHMODEL — `POST /api/session/{id}/model` and variant persistence.
//!
//! Upstream (probed live against 2.0.28, and identical in the frozen 1.18.31
//! v1 surface): the body is `{model: Model.Ref}` where `Model.Ref` is
//! `{id!, providerID!, variant?}`. A bare string is rejected with
//! `Expected Model.Ref`; a present-but-wrong-typed field names that field; a
//! missing `variant` persists `"default"` and an explicit `""` persists `""`.
//! A bogus session id answers 404 BEFORE body validation.
//!
//! The old handler parsed `model` with `as_str()`, so a correct client got a
//! misleading `Missing key at ["model"]` while the key was present, and every
//! switch stored `variant: "default"` — a switch from `high` to `low` was
//! silently recorded as a no-op difference.
//!
//! `variant` is not decoration: upstream merges `model.variants[variant]` into
//! the provider request options LAST (llm/request.ts:82,111-118). These tests
//! drive a real stub-provider turn and assert the bytes on the wire, so a
//! persisted-but-unapplied variant cannot pass.
use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use ocserve_http::{AppState, LlmRegistry, Payloads, Wires};
use serde_json::{Value, json};
use std::io::Read as _;
use std::sync::Arc;
use tower::ServiceExt;

fn spawn_capture_provider() -> (std::net::SocketAddr, Arc<std::sync::Mutex<Vec<String>>>) {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let bodies = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
    let sink = bodies.clone();
    std::thread::spawn(move || {
        for sock in listener.incoming().flatten() {
            let sink = sink.clone();
            std::thread::spawn(move || {
                let mut sock = sock;
                let _ = sock.set_read_timeout(Some(std::time::Duration::from_secs(5)));
                let mut got = Vec::new();
                let mut buf = [0u8; 8192];
                let head_end = loop {
                    match sock.read(&mut buf) {
                        Ok(0) => return,
                        Ok(k) => got.extend_from_slice(&buf[..k]),
                        Err(_) => return,
                    }
                    if let Some(pos) = got.windows(4).position(|w| w == b"\r\n\r\n") {
                        break pos + 4;
                    }
                };
                let head = String::from_utf8_lossy(&got[..head_end]).to_string();
                let clen = head
                    .lines()
                    .find_map(|l| {
                        let (k, v) = l.split_once(':')?;
                        k.eq_ignore_ascii_case("content-length")
                            .then(|| v.trim().parse::<usize>().ok())
                            .flatten()
                    })
                    .unwrap_or(0);
                while got.len() < head_end + clen {
                    match sock.read(&mut buf) {
                        Ok(0) => break,
                        Ok(k) => got.extend_from_slice(&buf[..k]),
                        Err(_) => break,
                    }
                }
                sink.lock()
                    .unwrap()
                    .push(String::from_utf8_lossy(&got[head_end..head_end + clen]).to_string());
                let body = "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\n\
                     cache-control: no-cache\r\nconnection: close\r\n\r\n\
                     data: {\"choices\":[{\"delta\":{\"content\":\"OK\"}}]}\n\n\
                     data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n\
                     data: [DONE]\n\n";
                let _ = std::io::Write::write_all(&mut sock, body.as_bytes());
                let _ = std::io::Write::flush(&mut sock);
            });
        }
    });
    (addr, bodies)
}

fn setup(addr: std::net::SocketAddr, sid: &str) -> Arc<AppState> {
    let dir =
        std::env::temp_dir().join(format!("ocserve-switchmodel-{}-{sid}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let db = ocserve_store::writer::db_path(&dir);
    let writer = Arc::new(ocserve_store::Writer::spawn(db.clone()).unwrap());
    let blobs = Arc::new(ocserve_store::BlobStore::new(dir.join("blobs")).unwrap());
    let llm = LlmRegistry {
        limits: Default::default(),
        endpoints: [("fake".into(), (format!("http://{addr}"), String::new()))]
            .into_iter()
            .collect(),
        pricing: Default::default(),
        default_model: ("fake".into(), "m".into()),
        systems: Default::default(),
        default_agent: "build".into(),
    };
    // The provider payload carries the variant options — the exact object
    // upstream merges into the request (llm/request.ts:82).
    let payloads = Payloads {
        config: json!({}),
        provider: json!({"providers": [{
            "id": "fake",
            "models": {"m": {"id": "m", "providerID": "fake",
                "variants": {"high": {"reasoningEffort": "high", "thinking": {"budget": 4096}},
                             "low": {"reasoningEffort": "low"}}}}
        }]}),
        ..Default::default()
    };
    let st = AppState::with_wiring_and_v2(
        None,
        payloads,
        Wires {
            db,
            blobs,
            writer,
            llm,
        },
        true,
    );
    ocserve_store::insert_session(
        &st.writer,
        &json!({
            "id": sid, "projectID": "global", "directory": "/w",
            "path": sid, "slug": sid, "title": "t", "version": "1",
            "time": {"created": 1, "updated": 2},
        }),
    )
    .unwrap();
    st
}

async fn post_json(app: &axum::Router, uri: &str, body: Value) -> axum::response::Response {
    app.clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(uri)
                .header("content-type", "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap()
}

async fn body_json(resp: axum::response::Response) -> Value {
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    serde_json::from_slice(&bytes).unwrap_or(Value::Null)
}

fn stored_model(st: &Arc<AppState>, sid: &str) -> Value {
    let conn = ocserve_store::pragma::open_reader(&st.db).unwrap();
    let raw: Option<String> = conn
        .query_row("SELECT model FROM session WHERE id = ?1", [sid], |r| {
            r.get(0)
        })
        .unwrap();
    serde_json::from_str(&raw.unwrap_or_default()).unwrap_or(Value::Null)
}

/// The reported bug: the object form is the contract, and it must persist
/// `variant` verbatim (probed: missing → "default", explicit "" → "").
#[tokio::test]
async fn switch_model_decodes_the_object_and_persists_the_variant() {
    let addr = spawn_capture_provider().0;
    let sid = "ses_swm0000000000000000000001";
    let st = setup(addr, sid);
    let app = ocserve_http::router(st.clone());

    let r = post_json(
        &app,
        &format!("/api/session/{sid}/model"),
        json!({"model": {"id": "m", "providerID": "fake", "variant": "high"}}),
    )
    .await;
    assert_eq!(r.status(), StatusCode::NO_CONTENT);
    assert_eq!(
        stored_model(&st, sid),
        json!({"id": "m", "providerID": "fake", "variant": "high"}),
        "variant must survive the switch (the pre-fix handler wrote 'default')"
    );

    // missing variant → upstream writes "default"
    let r = post_json(
        &app,
        &format!("/api/session/{sid}/model"),
        json!({"model": {"id": "m", "providerID": "fake"}}),
    )
    .await;
    assert_eq!(r.status(), StatusCode::NO_CONTENT);
    assert_eq!(stored_model(&st, sid)["variant"], "default");

    // explicit empty string is distinct (probed on 2.0.28)
    let r = post_json(
        &app,
        &format!("/api/session/{sid}/model"),
        json!({"model": {"id": "m", "providerID": "fake", "variant": ""}}),
    )
    .await;
    assert_eq!(r.status(), StatusCode::NO_CONTENT);
    assert_eq!(stored_model(&st, sid)["variant"], "");
}

/// Exact decode-error shapes, probed byte-for-byte against 2.0.28, and the
/// 404-before-decode ordering that a bogus session must keep.
#[tokio::test]
async fn switch_model_decode_errors_match_freeze() {
    let addr = spawn_capture_provider().0;
    let sid = "ses_swm0000000000000000000002";
    let st = setup(addr, sid);
    let app = ocserve_http::router(st.clone());

    let cases = [
        (
            json!({"model": "fake/m"}),
            "Expected Model.Ref\n  at [\"model\"]",
        ),
        (
            json!({"model": null}),
            "Expected Model.Ref\n  at [\"model\"]",
        ),
        (json!({}), "Missing key\n  at [\"model\"]"),
        (
            json!({"model": {"providerID": "fake"}}),
            "Missing key\n  at [\"model\"][\"id\"]",
        ),
        (
            json!({"model": {"id": "m"}}),
            "Missing key\n  at [\"model\"][\"providerID\"]",
        ),
        (
            json!({"model": {"id": 1, "providerID": "fake"}}),
            "Expected string\n  at [\"model\"][\"id\"]",
        ),
        (
            json!({"model": {"id": "m", "providerID": 1}}),
            "Expected string\n  at [\"model\"][\"providerID\"]",
        ),
        (
            json!({"model": {"id": "m", "providerID": "fake", "variant": 3}}),
            "Expected string\n  at [\"model\"][\"variant\"]",
        ),
        (
            json!({"model": {"id": "m", "providerID": "fake", "variant": null}}),
            "Expected string\n  at [\"model\"][\"variant\"]",
        ),
    ];
    for (body, want) in cases {
        let r = post_json(&app, &format!("/api/session/{sid}/model"), body.clone()).await;
        assert_eq!(r.status(), StatusCode::BAD_REQUEST, "body {body}");
        let j = body_json(r).await;
        assert_eq!(j["_tag"], "InvalidRequestError", "body {body}");
        assert_eq!(j["kind"], "Payload", "body {body}");
        assert_eq!(j["message"], want, "body {body}");
    }

    // unknown session: 404 wins over body decode (probed)
    let bogus = "ses_00000000000000000000000000";
    for body in [json!({}), json!({"model": null}), json!({"model": "x/y"})] {
        let r = post_json(&app, &format!("/api/session/{bogus}/model"), body.clone()).await;
        assert_eq!(r.status(), StatusCode::NOT_FOUND, "body {body}");
        let j = body_json(r).await;
        assert_eq!(j["_tag"], "SessionNotFoundError");
    }

    // extra keys are ignored (probed: 204)
    let r = post_json(
        &app,
        &format!("/api/session/{sid}/model"),
        json!({"model": {"id": "m", "providerID": "fake", "zzz": 1}}),
    )
    .await;
    assert_eq!(r.status(), StatusCode::NO_CONTENT);
}

/// The variant must reach the provider request. Upstream merges
/// `model.variants[variant]` LAST into the options; a switch to "high" that
/// changes nothing on the wire is the shape-without-mutation trap.
#[tokio::test]
async fn stored_variant_reaches_the_provider_request() {
    let (addr, bodies) = spawn_capture_provider();
    let sid = "ses_swm0000000000000000000003";
    let st = setup(addr, sid);
    let app = ocserve_http::router(st.clone());

    let r = post_json(
        &app,
        &format!("/api/session/{sid}/model"),
        json!({"model": {"id": "m", "providerID": "fake", "variant": "high"}}),
    )
    .await;
    assert_eq!(r.status(), StatusCode::NO_CONTENT);

    // payload has no model/variant: the session's stored selection decides
    let r = post_json(
        &app,
        &format!("/session/{sid}/message"),
        json!({"parts": [{"type": "text", "text": "hi"}], "agent": "build"}),
    )
    .await;
    assert_eq!(r.status(), StatusCode::OK);

    let sent = bodies.lock().unwrap().clone();
    assert_eq!(sent.len(), 1, "one provider request");
    let req: Value = serde_json::from_str(&sent[0]).expect("provider body is JSON");
    assert_eq!(
        req["reasoningEffort"], "high",
        "the stored variant's options must be merged into the request: {req}"
    );
    assert_eq!(req["thinking"]["budget"], 4096);

    // a request-level variant overrides the session's stored one
    let r = post_json(
        &app,
        &format!("/session/{sid}/message"),
        json!({"parts": [{"type": "text", "text": "hi"}],
               "variant": "low", "agent": "build"}),
    )
    .await;
    assert_eq!(r.status(), StatusCode::OK);
    let sent = bodies.lock().unwrap().clone();
    let req: Value = serde_json::from_str(&sent[sent.len() - 1]).unwrap();
    assert_eq!(req["reasoningEffort"], "low");
    assert!(
        req.get("thinking").is_none(),
        "a different variant's options must not leak: {req}"
    );
}

/// Negative control for the checker this suite relies on: the pre-fix body
/// (`as_str()`) must fail these cases, otherwise the tests above are theatre.
#[test]
fn pre_fix_string_parse_cannot_read_the_object() {
    let body = json!({"model": {"id": "m", "providerID": "fake", "variant": "high"}});
    assert!(
        body.get("model").and_then(|v| v.as_str()).is_none(),
        "the OLD extraction must see None here — that was the reported bug"
    );
}
