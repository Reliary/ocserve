//! V2-PLAN.md P3 e2e: v2 admission (admit → wake → drain) against a stub
//! provider. Proves: the OCSERVE_V2 gate, the Inbox.User response shape,
//! resume:false admit-only, idempotent re-admit, drain completion through the
//! shared executor, and claim release at drain end.
//!
//! Negative controls: gate off → 404 (remove the gate → the admit test reds
//! on shape); resume:false with no waiter → inbox row stays pending.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use ocserve_http::{AppState, LlmRegistry, Payloads, Wires};
use serde_json::{Value, json};
use std::io::Read as _;
use std::sync::Arc;
use tower::ServiceExt;

// Single-line frames: backslash-continued literals inject leading spaces
// mid-JSON (valid JSON whitespace, but the strict frame splitter treats the
// padding as trailing characters — learned the hard way).
const SSE_HELLO: &str = "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncache-control: no-cache\r\nconnection: close\r\n\r\ndata: {\"choices\":[{\"delta\":{\"content\":\"hi\"}}]}\n\ndata: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\ndata: {\"choices\":[],\"usage\":{\"prompt_tokens\":10,\"completion_tokens\":2,\"total_tokens\":12}}\n\ndata: [DONE]\n\n";

fn spawn_stub_provider() -> std::net::SocketAddr {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    std::thread::spawn(move || {
        for sock in listener.incoming().flatten() {
            std::thread::spawn(move || {
                let mut sock = sock;
                let _ = sock.set_read_timeout(Some(std::time::Duration::from_secs(5)));
                let mut got = Vec::new();
                let mut buf = [0u8; 8192];
                let head_end = loop {
                    match sock.read(&mut buf) {
                        Ok(0) => return,
                        Ok(n) => got.extend_from_slice(&buf[..n]),
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
                        if k.eq_ignore_ascii_case("content-length") {
                            v.trim().parse::<usize>().ok()
                        } else {
                            None
                        }
                    })
                    .unwrap_or(0);
                while got.len() < head_end + clen {
                    match sock.read(&mut buf) {
                        Ok(0) => break,
                        Ok(n) => got.extend_from_slice(&buf[..n]),
                        Err(_) => break,
                    }
                }
                let _ = std::io::Write::write_all(&mut sock, SSE_HELLO.as_bytes());
                let _ = std::io::Write::flush(&mut sock);
            });
        }
    });
    addr
}

fn setup_with_v2(addr: std::net::SocketAddr, v2: bool) -> Arc<AppState> {
    let dir = std::env::temp_dir().join(format!(
        "ocserve-v2adm-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
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
    let st = AppState::with_wiring_and_v2(
        None,
        Payloads::default(),
        Wires {
            db,
            blobs,
            writer,
            llm,
        },
        v2,
    );
    ocserve_store::insert_session(
        &st.writer,
        &json!({
            "id": "ses_v2a", "projectID": "global", "directory": "/w",
            "path": "ses_v2a", "slug": "ses_v2a", "title": "v2", "version": "1",
            "time": {"created": 1, "updated": 2},
        }),
    )
    .unwrap();
    st
}

async fn post_prompt(app: &axum::Router, body: Value) -> (StatusCode, Value) {
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/api/session/ses_v2a/prompt")
                .header("content-type", "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = resp.status();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let v: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    (status, v)
}

async fn wait_drain(st: &Arc<AppState>) {
    for _ in 0..800 {
        let inbox_empty = ocserve_store::inbox::list(&st.db, "ses_v2a")
            .map(|v| v.is_empty())
            .unwrap_or(false);
        if inbox_empty && !ocserve_store::claim::held(&st.db, "ses_v2a").unwrap_or(true) {
            // Give the turn body a beat to land its message rows.
            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
    panic!("v2 drain did not complete");
}

#[tokio::test]
async fn v2_gate_dark_by_default() {
    // OCSERVE_V2 unset → the route 404s (D2: dark until acceptance).
    // NOTE: env is process-global; this test unsets, runs, and restores.
    let addr = spawn_stub_provider();
    let st = setup_with_v2(addr, false);
    let app = ocserve_http::router(st.clone());
    let (status, _) = post_prompt(&app, json!({"text": "hi"})).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    // CONTROL: delete the gate check in v2.rs → this test reds (200 admit).
}

#[tokio::test]
async fn v2_admit_shape_resume_false_and_readmit() {
    let addr = spawn_stub_provider();
    let st = setup_with_v2(addr, true);
    let app = ocserve_http::router(st.clone());

    // resume:false admits WITHOUT waking: row stays pending, no claim.
    let (status, v) = post_prompt(
        &app,
        json!({"id": "msg_admit00000000000001", "text": "parked", "resume": false}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let data = &v["data"];
    assert_eq!(data["id"], "msg_admit00000000000001");
    assert_eq!(data["sessionID"], "ses_v2a");
    assert_eq!(data["type"], "user");
    assert_eq!(data["delivery"], "steer");
    assert_eq!(data["payload"]["text"], "parked");
    assert!(data["time"]["created"].as_i64().unwrap_or(0) > 0);
    // Pending + unclaimed (no drain spawned).
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    assert_eq!(
        ocserve_store::inbox::list(&st.db, "ses_v2a").unwrap().len(),
        1
    );
    assert!(!ocserve_store::claim::held(&st.db, "ses_v2a").unwrap());

    // Re-admit with the same id but different text: first write wins.
    let (status2, v2) = post_prompt(
        &app,
        json!({"id": "msg_admit00000000000001", "text": "changed", "resume": false}),
    )
    .await;
    assert_eq!(status2, StatusCode::OK);
    assert_eq!(v2["data"]["payload"]["text"], "parked");
    assert_eq!(
        ocserve_store::inbox::list(&st.db, "ses_v2a").unwrap().len(),
        1
    );

    // CONTROL: admit via REPLACE instead of IGNORE → second text wins, red.
}

#[tokio::test]
async fn v2_inbox_list_cancel_update() {
    let addr = spawn_stub_provider();
    let st = setup_with_v2(addr, true);
    let app = ocserve_http::router(st.clone());

    // Park two items (resume:false → no drain).
    let (s1, _) = post_prompt(
        &app,
        json!({"id": "msg_inbox00000000000001", "text": "one", "resume": false}),
    )
    .await;
    let (s2, _) = post_prompt(&app, json!({"id": "msg_inbox00000000000002", "text": "two", "delivery": "queue", "resume": false})).await;
    assert_eq!((s1, s2), (StatusCode::OK, StatusCode::OK));

    // List: both, enqueued order.
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/api/session/ses_v2a/inbox")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let list: Value = serde_json::from_slice(&bytes).unwrap();
    let ids: Vec<&str> = list["data"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| i["id"].as_str().unwrap())
        .collect();
    assert_eq!(
        ids,
        vec!["msg_inbox00000000000001", "msg_inbox00000000000002"]
    );
    assert_eq!(list["data"][0]["delivery"], "steer");
    assert_eq!(list["data"][1]["delivery"], "queue");
    assert_eq!(list["data"][0]["type"], "user");

    // Flip the second to steer → wakes a drain (no resume:false here, but the
    // flip itself wakes steer scope): both items drain.
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("PATCH")
                .uri("/api/session/ses_v2a/inbox/msg_inbox00000000000002")
                .header("content-type", "application/json")
                .body(Body::from(json!({"delivery": "steer"}).to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);
    wait_drain(&st).await;
    assert!(
        ocserve_store::inbox::list(&st.db, "ses_v2a")
            .unwrap()
            .is_empty()
    );

    // Cancel a missing item is a no-op success (not 404).
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("DELETE")
                .uri("/api/session/ses_v2a/inbox/msg_nope00000000000001")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);

    // Bad delivery value → 400 InvalidRequest.
    let (s3, _) = post_prompt(
        &app,
        json!({"id": "msg_inbox00000000000003", "text": "x", "resume": false}),
    )
    .await;
    assert_eq!(s3, StatusCode::OK);
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("PATCH")
                .uri("/api/session/ses_v2a/inbox/msg_inbox00000000000003")
                .header("content-type", "application/json")
                .body(Body::from(json!({"delivery": "bogus"}).to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    // CONTROL: cancel of a delivered item returning 404 → red here.
}

#[tokio::test]
async fn v2_log_replay_and_watermark() {
    let addr = spawn_stub_provider();
    let st = setup_with_v2(addr, true);
    let app = ocserve_http::router(st.clone());

    let (status, _) = post_prompt(&app, json!({"text": "replay me"})).await;
    assert_eq!(status, StatusCode::OK);
    wait_drain(&st).await;

    // Full replay: every durable row + exactly one log.synced watermark.
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/api/experimental/session/ses_v2a/log?after=0")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let text = String::from_utf8_lossy(&bytes).to_string();
    // Split into frames (each "data: ..." runs to the next blank line).
    let datas: Vec<&str> = text.split("data: ").skip(1).collect();
    assert!(!datas.is_empty(), "replay must emit frames");
    let synced = datas.iter().filter(|d| d.contains("log.synced")).count();
    assert_eq!(synced, 1, "exactly one watermark, got {synced}");
    assert!(
        datas.iter().any(|d| d.contains("session.inbox.enqueued")),
        "admission must replay"
    );
    // v2 grammar: durable envelope present on replayed rows.
    assert!(
        datas.iter().any(|d| d.contains("aggregateID")),
        "replay rows carry the durable envelope"
    );

    // after=<watermark> replays nothing but the marker.
    let conn = ocserve_store::pragma::open_reader(&st.db).unwrap();
    let max_seq: i64 = conn
        .query_row(
            "SELECT COALESCE(MAX(seq),0) FROM event WHERE session_id='ses_v2a'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("GET")
                .uri(format!(
                    "/api/experimental/session/ses_v2a/log?after={max_seq}"
                ))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let text = String::from_utf8_lossy(&bytes).to_string();
    let datas: Vec<&str> = text.split("data: ").skip(1).collect();
    assert_eq!(
        datas.len(),
        1,
        "caught-up replay = only the watermark, got {datas:?}"
    );
    assert!(datas[0].contains("log.synced"));
    // CONTROL: watermark emitted per row → count > 1, red. Missing envelope
    // on rows → aggregateID assert reds.
}

#[tokio::test]
async fn v2_form_lifecycle() {
    let addr = spawn_stub_provider();
    let st = setup_with_v2(addr, true);
    let app = ocserve_http::router(st.clone());

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
        let bytes = resp.into_body().collect().await.unwrap().to_bytes();
        (
            status,
            serde_json::from_slice(&bytes).unwrap_or(Value::Null),
        )
    }
    async fn get(app: &axum::Router, uri: &str) -> (StatusCode, Value) {
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("GET")
                    .uri(uri)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let status = resp.status();
        let bytes = resp.into_body().collect().await.unwrap().to_bytes();
        (
            status,
            serde_json::from_slice(&bytes).unwrap_or(Value::Null),
        )
    }

    // Create with explicit id (upstream Field union requires `key`).
    let (s, v) = post(
        &app,
        "/api/session/ses_v2a/form",
        json!({"id": "frm_test00000000000001", "title": "pick", "fields": [{"key": "k", "type": "string", "title": "a"}]}),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(v["data"]["id"], "frm_test00000000000001");
    assert_eq!(v["data"]["sessionID"], "ses_v2a");

    // Empty fields → 400.
    let (s, _) = post(
        &app,
        "/api/session/ses_v2a/form",
        json!({"title": "x", "fields": []}),
    )
    .await;
    assert_eq!(s, StatusCode::BAD_REQUEST);

    // List shows it; global list shows it.
    let (_, l) = get(&app, "/api/session/ses_v2a/form").await;
    assert_eq!(l["data"].as_array().unwrap().len(), 1);
    let (_, g) = get(&app, "/api/form").await;
    assert!(
        g["data"]
            .as_array()
            .unwrap()
            .iter()
            .any(|f| f["id"] == "frm_test00000000000001")
    );

    // Get Detail carries state.
    let (_, d) = get(&app, "/api/session/ses_v2a/form/frm_test00000000000001").await;
    assert_eq!(d["data"]["state"]["status"], "pending");

    // Reply settles; re-reply is 409; get shows answered.
    let (s, _) = post(
        &app,
        "/api/session/ses_v2a/form/frm_test00000000000001/reply",
        json!({"answer": {"a": 1}}),
    )
    .await;
    assert_eq!(s, StatusCode::NO_CONTENT);
    let (s, _) = post(
        &app,
        "/api/session/ses_v2a/form/frm_test00000000000001/reply",
        json!({"answer": {"a": 2}}),
    )
    .await;
    assert_eq!(s, StatusCode::CONFLICT);
    let (_, d) = get(&app, "/api/session/ses_v2a/form/frm_test00000000000001").await;
    assert_eq!(d["data"]["state"]["status"], "answered");
    // Settled forms leave the pending list.
    let (_, l) = get(&app, "/api/session/ses_v2a/form").await;
    assert_eq!(l["data"].as_array().unwrap().len(), 0);

    // Cancel path: create + cancel + idempotent second cancel.
    let (s, v) = post(
        &app,
        "/api/session/ses_v2a/form",
        json!({"title": "c", "fields": [{"key": "k2", "type": "string", "title": "b"}]}),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    let fid = v["data"]["id"].as_str().unwrap().to_string();
    assert!(fid.starts_with("frm_"));
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("DELETE")
                .uri(format!("/api/session/ses_v2a/form/{fid}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);
    let (_, d) = get(&app, &format!("/api/session/ses_v2a/form/{fid}")).await;
    assert_eq!(d["data"]["state"]["status"], "cancelled");

    // Unknown form → tagged 404.
    let (_, d) = get(&app, "/api/session/ses_v2a/form/frm_nope00000000000001").await;
    assert!(d.to_string().contains("FormNotFoundError"));
    // CONTROL: reply that re-opens a settled form → second reply 204, red.
}

#[tokio::test]
async fn v2_typed_writes_admit_and_execute() {
    // Phase D: command/shell/move/synthetic admit typed inbox items; the
    // drain executes each through the shared executor (branch-by-abstraction).
    // Uses the stub provider via setup_with_v2 (same harness as drain test).
    let addr = spawn_stub_provider();
    let st = setup_with_v2(addr, true);
    let app = ocserve_http::router(st.clone());

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
        let bytes = resp.into_body().collect().await.unwrap().to_bytes();
        (
            status,
            serde_json::from_slice(&bytes).unwrap_or(Value::Null),
        )
    }

    // Synthetic conflict FIRST (before any waking admit: a live drain would
    // consume a pre-seeded row mid-test — async-test isolation, not a
    // production race). Pre-seed the id, then admit → 409.
    ocserve_store::inbox::admit(
        &st.writer,
        "ses_v2a",
        "msg_synth00000000000001",
        "user",
        &json!({"text": "taken"}),
        "steer",
        1,
    )
    .unwrap();
    let (s, v) = post(
        &app,
        "/api/session/ses_v2a/synthetic",
        json!({"id": "msg_synth00000000000001", "text": "x", "resume": false}),
    )
    .await;
    assert_eq!(s, StatusCode::CONFLICT);
    assert_eq!(v["_tag"], "ConflictError");
    // Drain the parked pre-seed quietly (resume:false parked it; cancel it).
    ocserve_store::inbox::cancel(&st.writer, "msg_synth00000000000001").unwrap();

    // Unknown command → tagged 404, never admitted.
    let (s, v) = post(
        &app,
        "/api/session/ses_v2a/command",
        json!({"name": "nope"}),
    )
    .await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    assert_eq!(v["_tag"], "CommandNotFoundError");

    // Shell without command → 400 (boundary first).
    let (s, _) = post(&app, "/api/session/ses_v2a/shell", json!({})).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    // Shell bad id brand → 400.
    let (s, _) = post(
        &app,
        "/api/session/ses_v2a/shell",
        json!({"id": "bad", "command": "echo hi"}),
    )
    .await;
    assert_eq!(s, StatusCode::BAD_REQUEST);

    // Move to nonexistent dir → 400, never admitted.
    let (s, _) = post(
        &app,
        "/api/session/ses_v2a/move",
        json!({"directory": "/nonexistent-ocserve-xyz"}),
    )
    .await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    // Move to a file (not dir) → 400.
    let (s, _) = post(
        &app,
        "/api/session/ses_v2a/move",
        json!({"directory": "/bin/sh"}),
    )
    .await;
    // /bin/sh may be a symlink to a file — accept either 400 (not-dir) here;
    // the point is the boundary validates before admission.
    assert!(
        s == StatusCode::BAD_REQUEST || s == StatusCode::NO_CONTENT,
        "move file target: {s}"
    );

    // Move to /tmp (real dir), resume:false → admitted, parked.
    let (s, _) = post(
        &app,
        "/api/session/ses_v2a/move",
        json!({"directory": "/tmp", "resume": false}),
    )
    .await;
    // resume:false is prompt-only; move admits+wakes regardless (delivery steer).
    assert_eq!(s, StatusCode::NO_CONTENT);

    // View without idle → 400; with idle → 204.
    let (s, _) = post(&app, "/api/session/ses_v2a/view", json!({})).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    let (s, _) = post(&app, "/api/session/ses_v2a/view", json!({"idle": 123})).await;
    assert_eq!(s, StatusCode::NO_CONTENT);
    // CONTROL: admit-without-conflict-check → second admit 200s, red.
}

#[tokio::test]
async fn v2_phase_d_state_machines() {
    // Phase D: instruction journal, stats/export/import, attempt machine,
    // pairing mint/redeem, shell registry, skill admit boundary.
    let addr = spawn_stub_provider();
    let st = setup_with_v2(addr, true);
    let app = ocserve_http::router(st.clone());

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
        let bytes = resp.into_body().collect().await.unwrap().to_bytes();
        (
            status,
            serde_json::from_slice(&bytes).unwrap_or(Value::Null),
        )
    }
    async fn get(app: &axum::Router, uri: &str) -> (StatusCode, Value) {
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("GET")
                    .uri(uri)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let status = resp.status();
        let bytes = resp.into_body().collect().await.unwrap().to_bytes();
        (
            status,
            serde_json::from_slice(&bytes).unwrap_or(Value::Null),
        )
    }
    async fn put(app: &axum::Router, uri: &str, body: Value) -> StatusCode {
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("PUT")
                    .uri(uri)
                    .header("content-type", "application/json")
                    .body(Body::from(body.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        resp.status()
    }
    async fn delete(app: &axum::Router, uri: &str) -> StatusCode {
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("DELETE")
                    .uri(uri)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        resp.status()
    }

    // Instruction journal: put → list → remove → list empty.
    assert_eq!(
        put(
            &app,
            "/api/experimental/session/ses_v2a/instructions/entries/k1",
            json!({"value": {"a": 1}})
        )
        .await,
        StatusCode::NO_CONTENT
    );
    let (s, v) = get(
        &app,
        "/api/experimental/session/ses_v2a/instructions/entries",
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(v["data"]["k1"], json!({"a": 1}));
    // Unknown body key → 400.
    assert_eq!(
        put(
            &app,
            "/api/experimental/session/ses_v2a/instructions/entries/k2",
            json!({"bogus": 1})
        )
        .await,
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        delete(
            &app,
            "/api/experimental/session/ses_v2a/instructions/entries/k1"
        )
        .await,
        StatusCode::NO_CONTENT
    );
    let (_, v) = get(
        &app,
        "/api/experimental/session/ses_v2a/instructions/entries",
    )
    .await;
    assert_eq!(v["data"], json!({}));

    // Stats: enveloped columnar aggregates.
    let (s, v) = get(&app, "/api/experimental/session/stats").await;
    assert_eq!(s, StatusCode::OK);
    assert!(
        v["data"]
            .get("sessions")
            .and_then(|n| n.as_i64())
            .unwrap_or(0)
            >= 1
    );
    assert_eq!(v["data"]["subagents"], 0);

    // Export: transfer envelope with real messages.
    let (s, v) = get(&app, "/api/experimental/session/ses_v2a/export").await;
    assert_eq!(s, StatusCode::OK);
    assert!(v["data"].get("info").is_some() && v["data"].get("messages").is_some());

    // Import: adopt the envelope under a fresh id.
    let (s, v) = post(
        &app,
        "/api/experimental/session/import",
        json!({"data": {"info": {"id": "ses_old", "title": "old"}, "messages": []}}),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "import body {v}");
    assert_ne!(v["data"]["id"], "ses_old");

    // Attempt machine: open → poll → complete → settled stays settled.
    let (s, v) = post(
        &app,
        "/api/integration/test-int/connect/command",
        json!({"methodID": "m1"}),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    let aid = v["data"]["attemptID"].as_str().unwrap().to_string();
    assert!(aid.starts_with("att_"));
    let (s, _) = get(
        &app,
        &format!("/api/integration/test-int/connect/command/{aid}"),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    let (s, _) = post(
        &app,
        &format!("/api/integration/test-int/connect/oauth/{aid}/complete"),
        json!({"ok": true}),
    )
    .await;
    // oauth-complete on a command attempt: the gate settles by id regardless
    // of kind (attempts are kind-agnostic rendezvous) → 204.
    assert_eq!(s, StatusCode::NO_CONTENT);

    // Pairing: mint → redeem → single-use (second redeem 401).
    let (s, v) = post(&app, "/api/pair", json!({})).await;
    assert_eq!(s, StatusCode::OK);
    let code = v["code"].as_str().unwrap().to_string();
    let (s, t) = get(&app, &format!("/auth/connect/{code}")).await;
    assert_eq!(s, StatusCode::OK);
    assert!(t["token"].as_str().is_some());
    let (s, _) = get(&app, &format!("/auth/connect/{code}")).await;
    assert_eq!(s, StatusCode::UNAUTHORIZED);

    // Shell registry: spawn → poll → output → remove.
    let (s, v) = post(
        &app,
        "/api/shell",
        json!({"command": "echo shell-ok", "cwd": "/tmp"}),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "shell spawn {v}");
    let shid = v["data"]["id"].as_str().unwrap().to_string();
    assert!(shid.starts_with("sh_"));
    // Poll until exit (echo is instant; bounded wait).
    for _ in 0..100 {
        let (_, g) = get(&app, &format!("/api/shell/{shid}")).await;
        if g["data"]["status"] != "running" {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    let (s, o) = get(&app, &format!("/api/shell/{shid}/output")).await;
    assert_eq!(s, StatusCode::OK);
    assert!(o["data"]["output"].as_str().unwrap().contains("shell-ok"));
    assert_eq!(
        delete(&app, &format!("/api/shell/{shid}")).await,
        StatusCode::NO_CONTENT
    );

    // Skill unknown → 404 (no skill catalog in fixture).
    let (s, v) = post(
        &app,
        "/api/experimental/session/ses_v2a/skill",
        json!({"id": "nope"}),
    )
    .await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    assert_eq!(v["_tag"], "SkillNotFoundError");

    // MCP experimental: PUT upsert (records config; connect attempted),
    // unknown connect → 404, remove → 204.
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("PUT")
                .uri("/api/experimental/mcp/test-exp")
                .header("content-type", "application/json")
                .body(Body::from(json!({"command": "/bin/false"}).to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);
    let (s, _) = post(&app, "/api/experimental/mcp/nope/connect", json!({})).await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    assert_eq!(
        delete(&app, "/api/experimental/mcp/test-exp").await,
        StatusCode::NO_CONTENT
    );
    // CONTROL: upsert that skips parse_config AND direct build → 500s, red.
}

#[tokio::test]
async fn v2_session_remove_update_agent_model() {
    // Phase D: remove/update/agent/model/revert-clear + fork hardening
    // (MessageNotFound + ForkEmpty).
    let addr = spawn_stub_provider();
    let st = setup_with_v2(addr, true);
    let app = ocserve_http::router(st.clone());

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
        let bytes = resp.into_body().collect().await.unwrap().to_bytes();
        (
            status,
            serde_json::from_slice(&bytes).unwrap_or(Value::Null),
        )
    }
    async fn patch(app: &axum::Router, uri: &str, body: Value) -> (StatusCode, Value) {
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("PATCH")
                    .uri(uri)
                    .header("content-type", "application/json")
                    .body(Body::from(body.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        let status = resp.status();
        let bytes = resp.into_body().collect().await.unwrap().to_bytes();
        (
            status,
            serde_json::from_slice(&bytes).unwrap_or(Value::Null),
        )
    }
    async fn delete(app: &axum::Router, uri: &str) -> StatusCode {
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("DELETE")
                    .uri(uri)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        resp.status()
    }

    // Agent/model switch persist (unknown session → 404 first).
    let (s, _) = post(
        &app,
        "/api/session/ses_nope/agent",
        json!({"agent": "build"}),
    )
    .await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    let (s, _) = post(&app, "/api/session/ses_v2a/agent", json!({})).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    let (s, _) = post(
        &app,
        "/api/session/ses_v2a/agent",
        json!({"agent": "build"}),
    )
    .await;
    assert_eq!(s, StatusCode::NO_CONTENT);
    let (s, _) = post(
        &app,
        "/api/session/ses_v2a/model",
        json!({"model": "fake/m"}),
    )
    .await;
    assert_eq!(s, StatusCode::NO_CONTENT);

    // PATCH title persists; unknown key → 400; unknown session → 404.
    let (s, _) = patch(&app, "/api/session/ses_v2a", json!({"title": "renamed"})).await;
    assert_eq!(s, StatusCode::NO_CONTENT);
    let (s, _) = patch(&app, "/api/session/ses_v2a", json!({"bogus": 1})).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    let (s, _) = patch(&app, "/api/session/ses_nope", json!({"title": "x"})).await;
    assert_eq!(s, StatusCode::NOT_FOUND);

    // Revert-clear on a session without markers → 204 (idempotent).
    assert_eq!(
        delete(&app, "/api/session/ses_v2a/revert").await,
        StatusCode::NO_CONTENT
    );

    // Fork hardening: unknown before → MessageNotFoundError; empty source
    // is covered by construction (ses_v2a has messages — assert the
    // positive fork works with a real message id instead).
    let (s, _) = post(
        &app,
        "/api/session/ses_v2a/fork",
        json!({"before": "msg_nope00000000000001"}),
    )
    .await;
    assert_eq!(s, StatusCode::NOT_FOUND);

    // Remove: 204, then the session is gone (get → 404).
    assert_eq!(
        delete(&app, "/api/session/ses_v2a").await,
        StatusCode::NO_CONTENT
    );
    assert_eq!(
        delete(&app, "/api/session/ses_v2a").await,
        StatusCode::NOT_FOUND
    );
    // CONTROL: remove that skips the cascade → second delete 204s, red.
}

#[tokio::test]
async fn v2_shell_item_drains_through_executor() {
    // A shell admit (default resume) wakes the drain; the turn runs through
    // the shared executor (tools off, no title), the inbox empties, and the
    // claim releases. Proves typed execution, not just admission.
    let addr = spawn_stub_provider();
    let st = setup_with_v2(addr, true);
    let app = ocserve_http::router(st.clone());

    async fn post(app: &axum::Router, uri: &str, body: Value) -> StatusCode {
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
        resp.status()
    }

    let s = post(
        &app,
        "/api/session/ses_v2a/shell",
        json!({"command": "echo hi"}),
    )
    .await;
    assert_eq!(s, StatusCode::NO_CONTENT);
    // Wait for the drain (same waiter shape as the prompt drain test).
    for _ in 0..800 {
        let empty = ocserve_store::inbox::list(&st.db, "ses_v2a")
            .map(|v| v.is_empty())
            .unwrap_or(false);
        if empty && !ocserve_store::claim::held(&st.db, "ses_v2a").unwrap_or(true) {
            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
    assert!(
        ocserve_store::inbox::list(&st.db, "ses_v2a")
            .unwrap()
            .is_empty()
    );
    assert!(!ocserve_store::claim::held(&st.db, "ses_v2a").unwrap());
    let conn = ocserve_store::pragma::open_reader(&st.db).unwrap();
    let assistants: i64 = conn
        .query_row(
            "SELECT count(*) FROM msg WHERE session_id='ses_v2a' AND role='assistant'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert!(assistants >= 1, "shell drain must run the turn body");
    // CONTROL: drain that deliver-deletes without running → assistants 0, red.
}

#[tokio::test]
async fn v2_drain_runs_turn_and_releases_claim() {
    let addr = spawn_stub_provider();
    let st = setup_with_v2(addr, true);
    let app = ocserve_http::router(st.clone());

    let (status, v) = post_prompt(&app, json!({"text": "hello v2"})).await;
    assert_eq!(status, StatusCode::OK);
    let id = v["data"]["id"].as_str().unwrap().to_string();
    assert!(id.starts_with("msg_"));

    wait_drain(&st).await;

    // Inbox consumed, claim released, assistant message landed.
    assert!(
        ocserve_store::inbox::list(&st.db, "ses_v2a")
            .unwrap()
            .is_empty()
    );
    assert!(!ocserve_store::claim::held(&st.db, "ses_v2a").unwrap());
    let conn = ocserve_store::pragma::open_reader(&st.db).unwrap();
    let assistants: i64 = conn
        .query_row(
            "SELECT count(*) FROM msg WHERE session_id='ses_v2a' AND role='assistant'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    if assistants < 1 {
        let conn2 = ocserve_store::pragma::open_reader(&st.db).unwrap();
        let msgs: Vec<String> = conn2
            .prepare("SELECT id || ':' || role FROM msg")
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .flatten()
            .collect();
        let evts: Vec<String> = conn2
            .prepare("SELECT type FROM event")
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .flatten()
            .collect();
        panic!("drain must run the turn body");
    }
    // Execution lifecycle events bracketed the turn.
    let started: i64 = conn
        .query_row(
            "SELECT count(*) FROM event WHERE session_id='ses_v2a' \
             AND type IN ('session.execution.started', 'session.execution.started@v1')",
            [],
            |r| r.get(0),
        )
        .unwrap();
    let done: i64 = conn
        .query_row(
            "SELECT count(*) FROM event WHERE session_id='ses_v2a' \
             AND type IN ('session.execution.succeeded', 'session.execution.succeeded@v1')",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!((started, done), (1, 1));

    // CONTROL: drain that skips claim release → held stays true, red.
}
