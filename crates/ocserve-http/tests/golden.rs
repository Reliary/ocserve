//! Byte-golden tests: ocserve's wire output vs the freeze (PLAN §3, TESTING §5/§6).
//! Oracle: recordings under testdata/golden captured from upstream 1.18.31.
//! Volatile fields (event IDs, Date) are normalized before comparison — named,
//! never skipped (AGENTS.md §2.1).

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use tower::ServiceExt; // oneshot

fn app() -> axum::Router {
    ocserve_http::router(ocserve_http::AppState::new())
}

fn freeze_headers(h: &axum::http::HeaderMap) -> Vec<(&'static str, String)> {
    h.iter()
        .filter(|(k, _)| {
            matches!(
                k.as_str(),
                "content-type"
                    | "cache-control"
                    | "x-content-type-options"
                    | "x-accel-buffering"
                    | "content-length"
            )
        })
        .map(|(k, v)| {
            let name: &'static str = match k.as_str() {
                "content-type" => "content-type",
                "cache-control" => "cache-control",
                "x-content-type-options" => "x-content-type-options",
                "x-accel-buffering" => "x-accel-buffering",
                "content-length" => "content-length",
                _ => unreachable!(),
            };
            (name, v.to_str().unwrap_or_default().to_string())
        })
        .collect()
}

/// Finite-body request: full read (REST responses end).
async fn collect(req: Request<Body>) -> (StatusCode, Vec<(&'static str, String)>, bytes::Bytes) {
    let resp = app().oneshot(req).await.expect("router responds");
    let status = resp.status();
    let headers = freeze_headers(resp.headers());
    let body = resp.into_body().collect().await.unwrap().to_bytes();
    (status, headers, body)
}

/// SSE: never completes — read only the FIRST body frame, bounded (TESTING §6:
/// a hang is a failure, so the read itself is under a timeout).
async fn collect_first(
    req: Request<Body>,
) -> (StatusCode, Vec<(&'static str, String)>, bytes::Bytes) {
    let resp = app().oneshot(req).await.expect("router responds");
    let status = resp.status();
    let headers = freeze_headers(resp.headers());
    let mut body = resp.into_body();
    let first = tokio::time::timeout(std::time::Duration::from_secs(3), body.frame())
        .await
        .expect("first SSE frame within 3s")
        .expect("stream open")
        .expect("frame decodes");
    let bytes = first.into_data().unwrap_or_default();
    (status, headers, bytes)
}

/// GOLDEN: /global/health bytes are identical to the recorded upstream body.
#[tokio::test]
async fn health_bytes_match_upstream() {
    let (_s, _h, body) = collect(
        Request::builder()
            .uri("/global/health")
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    let golden = std::fs::read(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../testdata/golden/global_health.body"
    ))
    .expect("golden health body present");
    assert_eq!(
        body.as_ref(),
        &golden[..],
        "health body drifted from freeze"
    );
}

/// GOLDEN: 404 envelope bytes + status (recorded live from upstream).
#[tokio::test]
async fn notfound_envelope_matches_upstream() {
    let (status, _h, body) = collect(
        Request::builder()
            .uri("/session/ses_nonexistent")
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let golden = std::fs::read(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../testdata/golden/error_notfound.body"
    ))
    .expect("golden error body present");
    assert_eq!(body.as_ref(), &golden[..], "error envelope drifted");
}

/// GOLDEN: SSE header set is exactly the freeze set (PLAN §3). Missing/renamed
/// header = fail (axum defaults differ — insert overrides are asserted here).
#[tokio::test]
async fn sse_headers_match_freeze() {
    let (status, headers, _body) = collect_first(
        Request::builder()
            .uri("/global/event")
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    for (want_k, want_v) in ocserve_http::sse_expected_headers() {
        let got = headers.iter().find(|(k, _)| *k == want_k);
        match got {
            Some((_, v)) => assert_eq!(v, want_v, "header {want_k} drifted"),
            None => panic!("missing freeze header {want_k}"),
        }
    }
}

/// `/event` is the bare `Event` (`{id,type,properties}`), NOT an alias of
/// `/global/event` (`{payload}`). Live-probed 2026-10-11: freeze `/event`
/// emits `{"id":…,"type":"server.connected","properties":{}}`; the earlier
/// "byte-identical alias" claim (public.ts:155) was wrong and shipped a
/// divergent envelope. Headers still match across the group.
#[tokio::test]
async fn event_serves_bare_event_not_global_alias() {
    let (s1, h1, b1) = collect_first(
        Request::builder()
            .uri("/event")
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    let (s2, _h2, b2) = collect_first(
        Request::builder()
            .uri("/global/event")
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(s1, StatusCode::OK);
    assert_eq!(s1, s2, "status diverged");
    assert_eq!(h1, _h2, "header sets diverged");
    let bare = String::from_utf8_lossy(&b1);
    let first = bare.split("\n\n").next().expect("at least one frame");
    assert_eq!(
        normalize_ids(first),
        r#"data: {"id":"evt_<ID>","type":"server.connected","properties":{}}"#,
        "/event first frame drifted (bare Event)"
    );
    assert!(!bare.contains("\nid:"), "/event: id: lines forbidden");
    // global still wraps
    let global = String::from_utf8_lossy(&b2);
    let gfirst = global.split("\n\n").next().unwrap();
    assert!(
        gfirst.contains(r#""payload""#),
        "/global/event must wrap payload: {gfirst}"
    );
}

/// GOLDEN: first SSE frame bytes match upstream modulo the volatile event id.
/// Upstream: data: {"payload":{"id":"evt_<id>","type":"server.connected","properties":{}}}
/// with key order id,type,properties (serde_json preserve_order required).
#[tokio::test]
async fn sse_first_frame_matches_upstream() {
    let (_s, _h, body) = collect_first(
        Request::builder()
            .uri("/global/event")
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    let text = String::from_utf8_lossy(&body);
    let first = text.split("\n\n").next().expect("at least one frame");
    let norm = normalize_ids(first);
    assert_eq!(
        norm, r#"data: {"payload":{"id":"evt_<ID>","type":"server.connected","properties":{}}}"#,
        "first frame drifted from freeze"
    );
    // No id:/retry: lines anywhere in what we've seen (freeze fact §3)
    assert!(!text.contains("\nid:"), "id: lines are forbidden by freeze");
    assert!(
        !text.contains("retry:"),
        "retry: lines are forbidden by freeze"
    );
}

fn normalize_ids(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if s[i..].starts_with("evt_") {
            out.push_str("evt_");
            i += 4;
            let start = i;
            while i < bytes.len() && bytes[i].is_ascii_alphanumeric() {
                i += 1;
            }
            if i - start >= 20 {
                out.push_str("<ID>");
            } else {
                out.push_str(&s[start..i]);
            }
        } else {
            let ch = s[i..].chars().next().unwrap();
            out.push(ch);
            i += ch.len_utf8();
        }
    }
    out
}

/// Negative control for the frame test: normalization must NOT be a no-op
/// (if it were, the test could never fail on id drift — anti-theater §1).
#[test]
fn id_normalization_actually_normalizes() {
    let upstream = r#"data: {"payload":{"id":"evt_0f97ff8ab0017PVK23XpJUTuiH","type":"server.connected","properties":{}}}"#;
    let ours = r#"data: {"payload":{"id":"evt_0a1b2c3d4e5f6a7b8c9d0e1f2a","type":"server.connected","properties":{}}}"#;
    assert_eq!(normalize_ids(upstream), normalize_ids(ours));
    assert_ne!(
        normalize_ids(upstream),
        upstream,
        "normalization must change long ids"
    );
    // short ids (bug: fixed id strings) are NOT normalized — proves the 26-char
    // span in the frame test is doing real work
    assert_ne!(normalize_ids("evt_connected"), "evt_<ID>");
}

/// Session list returns [] on an empty server (M1), with 200 + JSON content-type.
#[tokio::test]
async fn session_list_empty_ok() {
    let (status, _h, body) = collect(
        Request::builder()
            .uri("/session")
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body.as_ref(), b"[]");
}

/// Session status returns exactly {} (recorded: len=2).
#[tokio::test]
async fn session_status_bytes_match_upstream() {
    let (_s, _h, body) = collect(
        Request::builder()
            .uri("/session/status")
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    let golden = std::fs::read(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../testdata/golden/session_status.body"
    ))
    .expect("golden status body present");
    assert_eq!(body.as_ref(), &golden[..]);
}

/// KILL CRITERION K-SSE-BYTES: 10s heartbeat cadence, shape byte-equal to
/// upstream (`id,type,properties` order), no id:/retry: lines.
/// Uses tokio paused time — no real sleeps (TESTING §6: hangs are failures,
/// so the clock is virtual and the read stays bounded).
#[tokio::test(start_paused = true)]
async fn heartbeat_cadence_and_shape() {
    let resp = app()
        .oneshot(
            Request::builder()
                .uri("/global/event")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .expect("router responds");
    let mut body = resp.into_body();

    // Bound must exceed the 10s cadence: paused-time auto-advance jumps to the
    // EARLIEST timer — a 3s timeout would fire before the heartbeat sleep.
    async fn read_frame(body: &mut Body) -> bytes::Bytes {
        tokio::time::timeout(std::time::Duration::from_secs(11), body.frame())
            .await
            .expect("frame within bound (11s virtual)")
            .expect("stream open")
            .expect("frame decodes")
            .into_data()
            .unwrap_or_default()
    }

    // frame 1: server.connected, immediate (would also trip an 11s bound only
    // if the stream were dead)
    let _f1 = read_frame(&mut body).await;
    let t1 = tokio::time::Instant::now();

    // frame 2: heartbeat must arrive at t=10s (paused clock auto-advances;
    // if cadence were wrong the timeout fires first and panics)
    let f2 = read_frame(&mut body).await;
    let elapsed = t1.elapsed();
    assert!(
        elapsed >= std::time::Duration::from_secs(10),
        "heartbeat early: {elapsed:?}"
    );
    let text = String::from_utf8_lossy(&f2);
    let norm = normalize_ids(text.trim_end_matches('\n'));
    assert_eq!(
        norm, r#"data: {"payload":{"id":"evt_<ID>","type":"server.heartbeat","properties":{}}}"#,
        "heartbeat shape drifted"
    );
    assert!(!text.contains("retry:"));
}

// ---- M3: message paging + experimental session search (TESTING §4) ----

async fn seed_session(st: &ocserve_http::AppState, sid: &str, n_msgs: usize) {
    let w = st.writer.clone();
    let blobs = st.blobs.clone();
    for i in 0..n_msgs {
        let info = serde_json::json!({
            "id": format!("msg_{i:04}"),
            "sessionID": sid,
            "role": if i % 2 == 0 { "user" } else { "assistant" },
            "time": {"created": 1700000000000 + i as i64},
        });
        let parts = vec![serde_json::json!({
            "id": format!("prt_{i:04}"),
            "sessionID": sid,
            "messageID": info["id"],
            "type": "text",
            "text": format!("line {i}"),
        })];
        ocserve_store::insert_message(&w, Some(&*blobs), sid, &info, &parts).unwrap();
    }
}

#[tokio::test]
async fn message_paging_limit_returns_last_n_ascending() {
    let st = ocserve_http::AppState::with_payloads(None, ocserve_http::Payloads::default());
    ocserve_store::insert_session(
        &st.writer,
        &serde_json::json!({
            "id": "ses_page",
            "projectID": "global",
            "directory": "/work",
            "path": "ses_page",
            "slug": "ses_page",
            "title": "paging",
            "version": "1",
            "time": {"created": 1, "updated": 2},
        }),
    )
    .unwrap();
    seed_session(&st, "ses_page", 120).await;
    let app = ocserve_http::router(st);

    // limit=50 → the LAST 50 messages, ascending (upstream live contract §1084)
    let req = Request::builder()
        .uri("/session/ses_page/message?limit=50")
        .body(Body::empty())
        .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    let msgs = v.as_array().unwrap();
    assert_eq!(msgs.len(), 50, "limit=50 yields 50");
    assert_eq!(msgs[0]["info"]["id"], "msg_0070", "starts at msg 70");
    assert_eq!(msgs[49]["info"]["id"], "msg_0119", "ends at last message");

    // no limit → full history
    let req = Request::builder()
        .uri("/session/ses_page/message")
        .body(Body::empty())
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(v.as_array().unwrap().len(), 120);
}

#[tokio::test]
async fn message_before_param_rejects_like_freeze() {
    // Freeze fact (§1090): upstream 1.18.31 rejects EVERY `before` value with
    // Effect HttpApi BadRequest body {"_tag":"BadRequest"} — parity, not a bug.
    let st = ocserve_http::AppState::with_payloads(None, ocserve_http::Payloads::default());
    ocserve_store::insert_session(
        &st.writer,
        &serde_json::json!({
            "id": "ses_before",
            "projectID": "global",
            "directory": "/work",
            "path": "ses_before",
            "slug": "ses_before",
            "title": "before",
            "version": "1",
            "time": {"created": 1, "updated": 2},
        }),
    )
    .unwrap();
    let app = ocserve_http::router(st);
    for q in [
        "before=anything",
        "before=0",
        "before=msg_x",
        "before=2026-01-01",
    ] {
        let req = Request::builder()
            .uri(format!("/session/ses_before/message?{q}"))
            .body(Body::empty())
            .unwrap();
        let resp = app.clone().oneshot(req).await.unwrap();
        assert_eq!(
            resp.status(),
            StatusCode::BAD_REQUEST,
            "{q} must 400 like upstream"
        );
        let bytes = resp.into_body().collect().await.unwrap().to_bytes();
        assert_eq!(
            std::str::from_utf8(&bytes).unwrap(),
            r#"{"_tag":"BadRequest"}"#,
            "{q} body byte-equal freeze"
        );
    }
}

#[tokio::test]
async fn experimental_session_search_substring_and_project_keys() {
    let st = ocserve_http::AppState::with_payloads(None, ocserve_http::Payloads::default());
    for (sid, title) in [
        ("ses_s1", "OpenCode v1 vs v2 comparison"),
        ("ses_s2", "unrelated work"),
    ] {
        ocserve_store::insert_session(
            &st.writer,
            &serde_json::json!({
                "id": sid,
                "projectID": "global",
                "directory": "/work",
                "path": sid,
                "slug": sid,
                "title": title,
                "version": "1",
                "time": {"created": 1, "updated": 2},
            }),
        )
        .unwrap();
    }
    let app = ocserve_http::router(st);
    let req = Request::builder()
        .uri("/experimental/session?search=opencode&roots=true&limit=50")
        .body(Body::empty())
        .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    let rows = v.as_array().unwrap();
    assert_eq!(rows.len(), 1, "case-insensitive substring match");
    assert_eq!(rows[0]["id"], "ses_s1");
    assert_eq!(rows[0]["project"]["id"], "global");
    assert_eq!(rows[0]["project"]["worktree"], "/");
    // oc-remote: "not a content search" — message text must NOT match
    let req = Request::builder()
        .uri("/experimental/session?search=zzznomatchxyz")
        .body(Body::empty())
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    assert_eq!(bytes.as_ref(), b"[]", "no match → empty array");
}

// ---- M5: prompt_async (oc-remote send path, freeze session.prompt_async) ----

#[tokio::test]
async fn prompt_async_returns_204_persists_user_message_and_404s_unknown() {
    use ocserve_http::{AppState, LlmRegistry, Payloads, Wires};

    // temp-backed store + a deliberately dead endpoint (background task must
    // fail AFTER persisting the user message — that ordering is the contract)
    let dir = std::env::temp_dir().join(format!(
        "ocserve-async-{}-{}",
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
    let st = AppState::with_wiring(
        None,
        Payloads::default(),
        Wires {
            db: db.clone(),
            blobs,
            writer,
            llm,
        },
    );
    ocserve_store::insert_session(
        &st.writer,
        &serde_json::json!({
            "id": "ses_async",
            "projectID": "global",
            "directory": "/work",
            "path": "ses_async",
            "slug": "ses_async",
            "title": "async",
            "version": "1",
            "time": {"created": 1, "updated": 2},
        }),
    )
    .unwrap();
    let app = ocserve_http::router(st.clone());

    // 1. unknown session → 404 freeze envelope (checked BEFORE spawn)
    let req = Request::builder()
        .method("POST")
        .uri("/session/ses_nope/prompt_async")
        .header("content-type", "application/json")
        .body(Body::from(r#"{"parts":[{"type":"text","text":"hi"}]}"#))
        .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    assert_eq!(
        std::str::from_utf8(&bytes).unwrap(),
        r#"{"name":"NotFoundError","data":{"message":"Session not found: ses_nope"}}"#
    );

    // 2. decode rejects an INVALID messageID with freeze's v1 Payload bytes
    // (field-probes.md: `Expected a string starting with "msg"`).
    let req = Request::builder()
        .method("POST")
        .uri("/session/ses_async/prompt_async")
        .header("content-type", "application/json")
        .body(Body::from(
            r#"{"messageID":"notamsg","parts":[{"type":"text","text":"x"}]}"#,
        ))
        .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::BAD_REQUEST,
        "bad messageID decodes to 400"
    );
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(v["name"], "BadRequest");
    assert_eq!(v["data"]["kind"], "Payload");
    assert_eq!(
        v["data"]["message"],
        "Expected a string starting with \"msg\", got \"notamsg\"\n  at [\"messageID\"]"
    );

    // 3. valid → 204, empty body, client `messageID` (exact wire key — the
    // 2026-10-09 double-prompt bug was reading `messageId` and silently
    // generating a new id), background persists the CLIENT id.
    let req = Request::builder()
        .method("POST")
        .uri("/session/ses_async/prompt_async")
        .header("content-type", "application/json")
        .body(Body::from(
            r#"{"messageID":"msg_client_0001","parts":[{"type":"text","text":"ping"}],
                "model":{"providerID":"fake","modelID":"m"},"agent":"build"}"#,
        ))
        .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::NO_CONTENT, "204 immediately");
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    assert!(bytes.is_empty(), "NoContent body must be empty");

    // 4. negative pin: a lowercase `messageId` key is an UNKNOWN key (freeze
    // drops it — onExcessProperty ignore) → server generates its own id; the
    // supplied value must NOT appear as a stored message id.
    let req = Request::builder()
        .method("POST")
        .uri("/session/ses_async/prompt_async")
        .header("content-type", "application/json")
        .body(Body::from(
            r#"{"messageId":"msg_lc_ignored0000000000000001",
                "parts":[{"type":"text","text":"lc"}]}"#,
        ))
        .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::NO_CONTENT,
        "lowercase body still 204s (decode passes)"
    );
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    assert!(bytes.is_empty());

    // background tasks persist the user messages (then fail at the dead
    // endpoint — logged, never panics: panic=abort would kill the test)
    let mut saw_user = false;
    for _ in 0..30 {
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        let req = Request::builder()
            .uri("/session/ses_async/message")
            .body(Body::empty())
            .unwrap();
        let resp = app.clone().oneshot(req).await.unwrap();
        let bytes = resp.into_body().collect().await.unwrap().to_bytes();
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        if let Some(arr) = v.as_array() {
            let ids: Vec<&str> = arr
                .iter()
                .filter_map(|m| m["info"]["id"].as_str())
                .collect();
            if ids.contains(&"msg_client_0001") {
                saw_user = true;
            }
            assert!(
                !ids.contains(&"msg_lc_ignored0000000000000001"),
                "lowercase messageId must be ignored (server-generated id instead)"
            );
        }
    }
    assert!(saw_user, "background prompt persisted the client messageID");
}

// ---- oc-remote contract family, Batch 1 (TESTING §4) ----

async fn batch1_state() -> (std::sync::Arc<ocserve_http::AppState>, axum::Router) {
    let st = ocserve_http::AppState::with_payloads(None, ocserve_http::Payloads::default());
    ocserve_store::insert_session(
        &st.writer,
        &serde_json::json!({
            "id": "ses_b1",
            "projectID": "global",
            "directory": "/work",
            "path": "ses_b1",
            "slug": "ses_b1",
            "title": "batch1",
            "version": "1",
            "time": {"created": 1, "updated": 2},
        }),
    )
    .unwrap();
    // one message + one text part
    let info = serde_json::json!({
        "id": "msg_b1",
        "sessionID": "ses_b1",
        "role": "user",
        "time": {"created": 100},
    });
    let parts = vec![serde_json::json!({
        "id": "prt_b1",
        "sessionID": "ses_b1",
        "messageID": "msg_b1",
        "type": "text",
        "text": "before-edit",
    })];
    ocserve_store::insert_message(&st.writer, Some(&*st.blobs), "ses_b1", &info, &parts).unwrap();
    // a todo row
    st.writer
        .write(vec![ocserve_store::WriteOp::Sql {
            sql: "INSERT INTO todo (id, session_id, content, status, priority, time_created, time_updated) \
                  VALUES ('td1', 'ses_b1', 'ship it', 'in_progress', 'high', 1, 2)"
                .into(),
            params: vec![],
        }])
        .unwrap();
    let app = ocserve_http::router(st.clone());
    (st, app)
}

async fn body_json(resp: axum::response::Response) -> serde_json::Value {
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    serde_json::from_slice(&bytes).unwrap()
}

#[tokio::test]
async fn session_rename_delete_children_todo_abort() {
    let (_st, app) = batch1_state().await;

    // PATCH rename → wire reflects new title
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("PATCH")
                .uri("/session/ses_b1")
                .header("content-type", "application/json")
                .body(Body::from(r#"{"title":"renamed"}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let v = body_json(resp).await;
    assert_eq!(v["title"], "renamed");

    // unknown → 404 freeze envelope
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("PATCH")
                .uri("/session/ses_nope")
                .header("content-type", "application/json")
                .body(Body::from(r#"{"title":"x"}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);

    // children → []
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/session/ses_b1/children")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(body_json(resp).await, serde_json::json!([]));

    // todo → shape {content,status,priority}
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/session/ses_b1/todo")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let todos = body_json(resp).await;
    assert_eq!(todos[0]["content"], "ship it");
    assert_eq!(todos[0]["priority"], "high");

    // abort with no active task → true (idempotent)
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/session/ses_b1/abort")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(body_json(resp).await, serde_json::json!(true));

    // DELETE session → true, then reads 404
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("DELETE")
                .uri("/session/ses_b1")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(body_json(resp).await, serde_json::json!(true));
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/session/ses_b1")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    // deleted session → 404 on message list too
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/session/ses_b1/message")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn message_and_part_mutations_roundtrip() {
    let (_st, app) = batch1_state().await;

    // part PATCH (native keys, matching path) → stored data replaced
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("PATCH")
                .uri("/session/ses_b1/message/msg_b1/part/prt_b1")
                .header("content-type", "application/json")
                .body(Body::from(
                    r#"{"id":"prt_b1","sessionID":"ses_b1","messageID":"msg_b1",
                        "type":"text","text":"after-edit"}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = resp.status().as_u16();
    let v = body_json(resp).await;
    assert_eq!(status, 200, "patch part failed: {v}");
    assert_eq!(v["text"], "after-edit");
    // read back via GET messages
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/session/ses_b1/message")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let msgs = body_json(resp).await;
    assert_eq!(msgs[0]["parts"][0]["text"], "after-edit", "PATCH persisted");

    // mismatched ids → 400 (v1 rejects)
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("PATCH")
                .uri("/session/ses_b1/message/msg_b1/part/prt_b1")
                .header("content-type", "application/json")
                .body(Body::from(
                    r#"{"id":"prt_b1","sessionID":"ses_OTHER","messageID":"msg_b1","type":"text"}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);

    // DELETE part → true; gone from read
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("DELETE")
                .uri("/session/ses_b1/message/msg_b1/part/prt_b1")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(body_json(resp).await, serde_json::json!(true));
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/session/ses_b1/message")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let msgs = body_json(resp).await;
    assert_eq!(msgs[0]["parts"].as_array().map(|p| p.len()).unwrap_or(0), 0);

    // DELETE message → true; message gone
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("DELETE")
                .uri("/session/ses_b1/message/msg_b1")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(body_json(resp).await, serde_json::json!(true));
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/session/ses_b1/message")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let msgs = body_json(resp).await;
    assert_eq!(msgs.as_array().map(|m| m.len()).unwrap_or(99), 0);

    // DELETE unknown part → 404 envelope
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("DELETE")
                .uri("/session/ses_b1/message/msg_gone/part/prt_gone")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}

// ---- oc-remote contract family, Batch 2 (file/find/question) ----

#[tokio::test]
async fn file_find_question_routes() {
    // base directory = AppState paths.directory = /work (with_payloads);
    // scope_path canonicalizes — create a real dir the state can see? paths
    // are env-derived: use HOME-based temp instead by building state normally
    // and pointing requests at paths that exist under the canonical base.
    let st = ocserve_http::AppState::with_payloads(None, ocserve_http::Payloads::default());
    let base = std::path::Path::new(st.paths["directory"].as_str().unwrap_or("/"));
    // with_payloads directory = current_dir (canonicalizable). Seed a file
    // under a unique subdir we can safely write.
    let seed = base.join(format!("ocserve-file-test-{}", std::process::id()));
    std::fs::create_dir_all(&seed).unwrap();
    std::fs::write(seed.join("hello.txt"), "hi there").unwrap();
    std::fs::create_dir_all(seed.join("sub")).unwrap();
    std::fs::write(seed.join("sub/nested.rs"), "fn deep() {}").unwrap();
    let rel = format!("{}/", seed.file_name().unwrap().to_string_lossy());

    let app = ocserve_http::router(st.clone());

    // list directory
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .uri(format!("/file?path={rel}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let v = body_json(resp).await;
    let names: Vec<&str> = v
        .as_array()
        .unwrap()
        .iter()
        .map(|n| n["name"].as_str().unwrap())
        .collect();
    assert!(names.contains(&"hello.txt"), "listing: {names:?}");
    assert!(names.contains(&"sub"), "listing: {names:?}");
    let node = v.as_array().unwrap()[0].clone();
    for k in ["name", "path", "type", "absolute", "ignored"] {
        assert!(node.get(k).is_some(), "FileNode missing {k}");
    }

    // file content
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .uri(format!("/file/content?path={rel}hello.txt"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let v = body_json(resp).await;
    assert_eq!(v["type"], "text");
    assert_eq!(v["content"], "hi there");

    // scope escape → 400
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/file/content?path=../../../etc/passwd")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);

    // find/file substring
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .uri(format!("/find/file?query={}&limit=100", "nested"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let v = body_json(resp).await;
    let hits: Vec<&str> = v
        .as_array()
        .unwrap()
        .iter()
        .map(|x| x.as_str().unwrap())
        .collect();
    assert!(
        hits.iter().any(|h| h.ends_with("nested.rs")),
        "find/file hits: {hits:?}"
    );

    // find text
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/find?pattern=hi%20there")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let v = body_json(resp).await;
    let arr = v.as_array().unwrap();
    assert!(!arr.is_empty(), "grep found our file");
    // freeze SearchMatch shape: {path:{text}, lines:{text}, line_number,
    // absolute_offset, submatches:[{match:{text},start,end}]} (ripgrep.ts:56-72)
    assert!(
        arr.iter().any(|m| m["path"]["text"]
            .as_str()
            .unwrap_or("")
            .ends_with("hello.txt")),
        "our file in matches: {arr:?}"
    );
    assert!(arr[0]["line_number"].as_i64().is_some());
    assert!(arr[0]["lines"]["text"].as_str().is_some());
    assert!(arr[0]["submatches"].as_array().is_some());
    assert!(arr[0]["submatches"][0]["match"]["text"].as_str().is_some());

    // question polls → [], reply/reject → 404 (nothing pending, honest)
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/question")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(body_json(resp).await, serde_json::json!([]));
    // bad prefix decodes first → 400 Params (freeze); ocserve previously 404'd
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/question/q1/reply")
                .header("content-type", "application/json")
                .body(Body::from(r#"{"answers":[["yes"]]}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST, "bad prefix -> 400");
    // valid prefix, unknown id → 404 tagged
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/question/que_unknown1234567890123456/reject")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);

    std::fs::remove_dir_all(&seed).ok();
}

// ---- question tool rendezvous (v1 Question service port) ----

#[tokio::test]
async fn question_pending_reply_reject_flow() {
    let st = ocserve_http::AppState::with_payloads(None, ocserve_http::Payloads::default());
    ocserve_store::insert_session(
        &st.writer,
        &serde_json::json!({
            "id": "ses_q",
            "projectID": "global",
            "directory": "/work",
            "path": "ses_q",
            "slug": "ses_q",
            "title": "q",
            "version": "1",
            "time": {"created": 1, "updated": 2},
        }),
    )
    .unwrap();
    let app = ocserve_http::router(st.clone());

    // pending via the same register path the runner uses
    let request = serde_json::json!({
        "id": "que_test01",
        "sessionID": "ses_q",
        "questions": [{
            "question": "Ship it?", "header": "Ship",
            "options": [{"label": "Yes", "description": "go"}, {"label": "No", "description": "stop"}],
            "multiple": false, "custom": true
        }],
        "tool": {"messageID": "msg_x", "callID": "call_x"},
    });
    let (rx, _guard) = st.question_gate.register("que_test01", request.clone());

    // GET /question shows the pending request (oc-remote poll)
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/question")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let v = body_json(resp).await;
    assert_eq!(v[0]["id"], "que_test01");
    assert_eq!(v[0]["sessionID"], "ses_q");
    assert_eq!(v[0]["questions"][0]["question"], "Ship it?");
    assert_eq!(v[0]["tool"]["callID"], "call_x");

    // bad answers shape → 400
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/question/que_test01/reply")
                .header("content-type", "application/json")
                .body(Body::from(r#"{"answers":"nope"}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);

    // reply → true, resolves the runner's receiver with the answers
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/question/que_test01/reply")
                .header("content-type", "application/json")
                .body(Body::from(r#"{"answers":[["Yes"]]}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(body_json(resp).await, serde_json::json!(true));
    match rx.await.unwrap() {
        ocserve_core::question::Outcome::Answers(a) => {
            assert_eq!(a, vec![vec!["Yes".to_string()]])
        }
        _ => panic!("expected answers"),
    }
    // list empty; double reply → 404
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/question")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(body_json(resp).await, serde_json::json!([]));
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/question/que_test01/reply")
                .header("content-type", "application/json")
                .body(Body::from(r#"{"answers":[]}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);

    // reject flow: register → reject (empty body) → Outcome::Rejected
    let (rx2, _g2) = st.question_gate.register(
        "que_test02",
        serde_json::json!({"id": "que_test02", "sessionID": "ses_q", "questions": []}),
    );
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/question/que_test02/reject")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(body_json(resp).await, serde_json::json!(true));
    assert!(matches!(
        rx2.await.unwrap(),
        ocserve_core::question::Outcome::Rejected
    ));
}

// ---- oc-remote Batch 4: command expansion + shell + busy ----

#[test]
fn command_expansion_matches_v1_semantics() {
    // $ARGUMENTS verbatim
    assert_eq!(
        ocserve_http::expand_command("Review $ARGUMENTS carefully", "the diff --staged"),
        "Review the diff --staged carefully"
    );
    // v1: the LAST placeholder position swallows args[position-1..] joined —
    // with $2 max, $2 = "B C D" and $1 = "A" (port-faithful, not intuitive)
    assert_eq!(
        ocserve_http::expand_command("fix $2 with $1", "A B C D"),
        "fix B C D with A"
    );
    // last position joins remaining args
    assert_eq!(
        ocserve_http::expand_command("run $1", "one two three"),
        "run one two three"
    );
    // no placeholders + args → appended
    assert_eq!(
        ocserve_http::expand_command("plain template", "extra args"),
        "plain template\n\nextra args"
    );
    // no placeholders + empty args → untouched (trimmed)
    assert_eq!(ocserve_http::expand_command("  plain  ", ""), "plain");
    // quoted args split like v1 argsRegex
    assert_eq!(
        ocserve_http::split_command_args(r#"one "two three" 'four five' six"#),
        vec!["one", "two three", "four five", "six"]
    );
    // missing positional → empty string, final result trimmed (v1)
    assert_eq!(ocserve_http::expand_command("x $5", "a"), "x");
}

#[tokio::test]
async fn shell_runs_direct_and_records_messages() {
    let st = ocserve_http::AppState::with_payloads(None, ocserve_http::Payloads::default());
    ocserve_store::insert_session(
        &st.writer,
        &serde_json::json!({
            "id": "ses_shell",
            "projectID": "global",
            "directory": "/work",
            "path": "ses_shell",
            "slug": "ses_shell",
            "title": "shell",
            "version": "1",
            "time": {"created": 1, "updated": 2},
        }),
    )
    .unwrap();
    let app = ocserve_http::router(st.clone());

    // missing command → 400
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/session/ses_shell/shell")
                .header("content-type", "application/json")
                .body(Body::from(r#"{"agent":"build"}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);

    // direct execution (NO model call — works without an LLM endpoint)
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/session/ses_shell/shell")
                .header("content-type", "application/json")
                .body(Body::from(r#"{"agent":"build","command":"echo SHELL_OK"}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    let v = body_json(resp).await;
    assert_eq!(v["info"]["role"], "assistant");
    assert_eq!(v["parts"][0]["type"], "tool");
    assert_eq!(v["parts"][0]["tool"], "bash");
    assert_eq!(v["parts"][0]["state"]["status"], "completed");
    assert_eq!(v["parts"][0]["state"]["output"], "SHELL_OK\n");
    assert_eq!(v["parts"][0]["state"]["title"], "");
    assert_eq!(v["parts"][0]["state"]["metadata"]["output"], "SHELL_OK\n");

    // history: synthetic user text part + assistant bash part
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/session/ses_shell/message")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let msgs = body_json(resp).await;
    let arr = msgs.as_array().unwrap();
    assert_eq!(arr.len(), 2);
    // Full-history order is upstream's canonical (time_created, id) tuple
    // (message-v2.ts older() helper): same-ms ties order by id, NOT
    // insertion — locate by role/content instead of index.
    let user = arr
        .iter()
        .find(|m| m["info"]["role"] == "user")
        .expect("synthetic user message");
    let asst = arr
        .iter()
        .find(|m| m["info"]["role"] == "assistant")
        .expect("assistant message");
    assert_eq!(
        user["parts"][0]["text"],
        "The following tool was executed by the user"
    );
    assert_eq!(user["parts"][0]["synthetic"], true);
    assert_eq!(asst["parts"][0]["tool"], "bash");
    // role-located, not index: same-ms id ties order deterministically per
    // run but the ids themselves are generated (parallel-suite flake caught)
    assert_eq!(
        asst["info"]["cost"].as_f64(),
        Some(0.0),
        "shell never costs model tokens"
    );
}

#[tokio::test]
async fn command_unknown_400_emits_session_error_and_lists_available() {
    let st = ocserve_http::AppState::with_payloads(None, ocserve_http::Payloads::default());
    ocserve_store::insert_session(
        &st.writer,
        &serde_json::json!({
            "id": "ses_cmd",
            "projectID": "global",
            "directory": "/work",
            "path": "ses_cmd",
            "slug": "ses_cmd",
            "title": "cmd",
            "version": "1",
            "time": {"created": 1, "updated": 2},
        }),
    )
    .unwrap();
    let app = ocserve_http::router(st.clone());
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/session/ses_cmd/command")
                .header("content-type", "application/json")
                .body(Body::from(r#"{"command":"nope","arguments":""}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    let v = body_json(resp).await;
    // empty command list (Payloads::default) → no hint suffix
    assert!(
        v["data"]["message"]
            .as_str()
            .unwrap()
            .starts_with("Command not found: \"nope\""),
        "{v}"
    );
    // session.error persisted (durable) — read the event table
    let events = st.db.clone();
    let conn = ocserve_store::pragma::open_reader(&events).unwrap();
    let n: i64 = conn
        .query_row(
            "SELECT count(*) FROM event WHERE type = 'session.error'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert!(
        n >= 1,
        "session.error durable event emitted (oc-remote toast path)"
    );
}

#[tokio::test]
async fn command_expands_and_persists_user_message_before_llm_fails() {
    use ocserve_http::{AppState, LlmRegistry, Payloads, Wires};
    let dir = std::env::temp_dir().join(format!(
        "ocserve-cmd-{}-{}",
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
    let payloads = Payloads {
        command: vec![serde_json::json!({
        "name": "review",
        "description": "Review changes",
        "hints": ["$ARGUMENTS"],
        "source": "builtin",
        "template": "You are reviewing: $ARGUMENTS\nBe terse.",
        })],
        ..Default::default()
    };
    let st = AppState::with_wiring(
        None,
        payloads,
        Wires {
            db: db.clone(),
            blobs,
            writer,
            llm,
        },
    );
    ocserve_store::insert_session(
        &st.writer,
        &serde_json::json!({
            "id": "ses_cmd2",
            "projectID": "global",
            "directory": "/work",
            "path": "ses_cmd2",
            "slug": "ses_cmd2",
            "title": "cmd2",
            "version": "1",
            "time": {"created": 1, "updated": 2},
        }),
    )
    .unwrap();
    let app = ocserve_http::router(st.clone());

    // known command with args → expansion persisted as the user message
    // (LLM endpoint is dead → run_prompt fails AFTER persisting — same
    // pattern as the prompt_async test; the assertion is the expansion)
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/session/ses_cmd2/command")
                .header("content-type", "application/json")
                .body(Body::from(r#"{"command":"review","arguments":"PR 42"}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status().as_u16(), 500, "dead endpoint after persist");

    let msgs = ocserve_store::load_messages(&db, "ses_cmd2", None).unwrap();
    assert!(
        !msgs.is_empty(),
        "user message persisted before provider failure"
    );
    let text = msgs[0].1[0]["text"].as_str().unwrap();
    assert_eq!(text, "You are reviewing: PR 42\nBe terse.");
}
