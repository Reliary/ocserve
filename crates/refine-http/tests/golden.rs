//! Byte-golden tests: refine's wire output vs the freeze (PLAN §3, TESTING §5/§6).
//! Oracle: recordings under testdata/golden captured from upstream 1.18.31.
//! Volatile fields (event IDs, Date) are normalized before comparison — named,
//! never skipped (AGENTS.md §2.1).

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use tower::ServiceExt; // oneshot

fn app() -> axum::Router {
    refine_http::router(refine_http::AppState::new())
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
    for (want_k, want_v) in refine_http::sse_expected_headers() {
        let got = headers.iter().find(|(k, _)| *k == want_k);
        match got {
            Some((_, v)) => assert_eq!(v, want_v, "header {want_k} drifted"),
            None => panic!("missing freeze header {want_k}"),
        }
    }
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

async fn seed_session(st: &refine_http::AppState, sid: &str, n_msgs: usize) {
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
        refine_store::insert_message(&w, Some(&*blobs), sid, &info, &parts).unwrap();
    }
}

#[tokio::test]
async fn message_paging_limit_returns_last_n_ascending() {
    let st = refine_http::AppState::with_payloads(None, refine_http::Payloads::default());
    refine_store::insert_session(
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
    let app = refine_http::router(st);

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
    let st = refine_http::AppState::with_payloads(None, refine_http::Payloads::default());
    refine_store::insert_session(
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
    let app = refine_http::router(st);
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
    let st = refine_http::AppState::with_payloads(None, refine_http::Payloads::default());
    for (sid, title) in [
        ("ses_s1", "OpenCode v1 vs v2 comparison"),
        ("ses_s2", "unrelated work"),
    ] {
        refine_store::insert_session(
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
    let app = refine_http::router(st);
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
    use refine_http::{AppState, LlmRegistry, Payloads, Wires};

    // temp-backed store + a deliberately dead endpoint (background task must
    // fail AFTER persisting the user message — that ordering is the contract)
    let dir = std::env::temp_dir().join(format!(
        "refine-async-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let db = refine_store::writer::db_path(&dir);
    let writer = std::sync::Arc::new(refine_store::Writer::spawn(db.clone()).unwrap());
    let blobs = std::sync::Arc::new(refine_store::BlobStore::new(dir.join("blobs")).unwrap());
    let llm = LlmRegistry {
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
    refine_store::insert_session(
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
    let app = refine_http::router(st.clone());

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

    // 2. valid → 204, empty body, client messageId honored, background persists
    let req = Request::builder()
        .method("POST")
        .uri("/session/ses_async/prompt_async")
        .header("content-type", "application/json")
        .body(Body::from(
            r#"{"messageId":"msg_client_0001","parts":[{"type":"text","text":"ping"}],
                "model":{"providerID":"fake","modelID":"m"},"agent":"build"}"#,
        ))
        .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::NO_CONTENT, "204 immediately");
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    assert!(bytes.is_empty(), "NoContent body must be empty");

    // background task persists the user message (then fails at the dead
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
        if let Some(arr) = v.as_array()
            && arr.iter().any(|m| m["info"]["id"] == "msg_client_0001")
        {
            saw_user = true;
            break;
        }
    }
    assert!(saw_user, "background prompt persisted the user message");
}
