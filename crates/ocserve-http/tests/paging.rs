//! Cursor paging, status, and lifecycle (W1/W2/W3) — freeze bytes from
//! testdata/golden/message_page_contract.json + status_busy.body.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use ocserve_http::{AppState, LlmRegistry, Payloads, Wires};
use tower::ServiceExt; // oneshot

fn state_with(endpoints: [(&str, &str); 1]) -> (std::sync::Arc<AppState>, axum::Router) {
    let dir = std::env::temp_dir().join(format!(
        "ocserve-pg-{}-{}",
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
        endpoints: endpoints
            .into_iter()
            .map(|(k, v)| (k.to_string(), (v.to_string(), String::new())))
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
            db,
            blobs,
            writer,
            llm,
        },
    );
    let app = ocserve_http::router(st.clone());
    (st, app)
}

fn seed_session(st: &AppState, sid: &str) {
    ocserve_store::insert_session(
        &st.writer,
        &serde_json::json!({
            "id": sid, "projectID": "global", "directory": "/w", "path": sid,
            "slug": sid, "title": sid, "version": "1",
            "time": {"created": 1, "updated": 2},
        }),
    )
    .unwrap();
}

fn seed_msgs(st: &AppState, sid: &str, n: usize) {
    let mut ops: Vec<ocserve_store::WriteOp> = Vec::new();
    for i in 0..n {
        ops.push(ocserve_store::WriteOp::Sql {
            sql: "INSERT INTO msg (id, session_id, role, seq, time_created, info) VALUES (?1, ?2, 'user', ?3, ?4, ?5)".into(),
            params: vec![
                format!("msg_{i:04}").into(),
                sid.into(),
                ((i + 1) as i64).into(),
                (1_700_000_000_000i64 + i as i64).into(),
                format!("{{\"id\":\"msg_{i:04}\",\"sessionID\":\"{sid}\",\"role\":\"user\"}}").into(),
            ],
        });
    }
    st.writer.write(ops).unwrap();
}

async fn get(app: &axum::Router, uri: &str) -> (StatusCode, Vec<u8>, axum::http::HeaderMap) {
    let resp = app
        .clone()
        .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = resp.status();
    let headers = resp.headers().clone();
    let bytes = resp
        .into_body()
        .collect()
        .await
        .unwrap()
        .to_bytes()
        .to_vec();
    (status, bytes, headers)
}

#[test]
fn cursor_bytes_match_upstream_probe() {
    // Exact pair recorded from upstream 1.18.31 (probe 2026-10-02):
    // X-Next-Cursor: eyJpZCI6Im1zZ18wZmMyODRkYWQwMDFnalBCTktEam1BcE1HaSIsInRpbWUiOjE3OTA5MzY4OTUwNjV9
    // true pair decoded from the probe header itself (id verified by decode)
    let id = "msg_0fc284dad001gjPBNKDjmApMGi";
    let time = 1790936895065i64;
    assert_eq!(
        ocserve_store::encode_cursor(id, time),
        "eyJpZCI6Im1zZ18wZmMyODRkYWQwMDFnalBCTktEam1BcE1HaSIsInRpbWUiOjE3OTA5MzY4OTUwNjV9",
        "cursor must be byte-identical to upstream MessageV2.cursor"
    );
    let (did, dtime) = ocserve_store::decode_cursor(
        "eyJpZCI6Im1zZ18wZmMyODRkYWQwMDFnalBCTktEam1BcE1HaSIsInRpbWUiOjE3OTA5MzY4OTUwNjV9",
    )
    .unwrap();
    assert_eq!((did.as_str(), dtime), (id, time));
    assert!(ocserve_store::decode_cursor("garbage").is_err());
    assert!(
        ocserve_store::decode_cursor(&ocserve_store::encode_cursor("ses_x", 1)).is_err(),
        "non-msg_ ids rejected"
    );
}

#[tokio::test]
async fn page_headers_cursor_follow_and_streamed_body() {
    let (_st, _app) = state_with([("fake", "http://127.0.0.1:9")]);
    // (state dropped — rebuild with st kept)
    let dir = std::env::temp_dir().join(format!("ocserve-pg2-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let db = ocserve_store::writer::db_path(&dir);
    let writer = std::sync::Arc::new(ocserve_store::Writer::spawn(db.clone()).unwrap());
    let blobs = std::sync::Arc::new(ocserve_store::BlobStore::new(dir.join("blobs")).unwrap());
    let llm = LlmRegistry {
        limits: std::collections::HashMap::new(),
        endpoints: [("fake".into(), ("http://127.0.0.1:9".into(), String::new()))]
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
            db,
            blobs,
            writer,
            llm,
        },
    );
    seed_session(&st, "ses_p");
    seed_msgs(&st, "ses_p", 12);
    let app = ocserve_http::router(st.clone());

    // page 1: newest 5, ASC, headers present. Content-Length policy (F9
    // amendment — the OOM class lives in the UNBOUNDED full-history
    // stream, not in paged bodies): a paged response may be a bounded
    // materialization (memo cap 4 MiB, enforced in PageMemoState::put),
    // and if it carries Content-Length it must PROVE the bound. The
    // full-history no-CL invariant is asserted below on the same
    // session (negative control: full history never enters the memo).
    let (s, b, h) = get(&app, "/session/ses_p/message?limit=5").await;
    assert_eq!(s, 200);
    if let Some(cl) = h.get(axum::http::header::CONTENT_LENGTH) {
        let n: usize = cl.to_str().unwrap().parse().unwrap();
        assert!(
            n <= ocserve_http::PAGE_MEMO_ENTRY_CAP,
            "paged Content-Length must respect the memo entry cap: {n}"
        );
    }
    let v: serde_json::Value = serde_json::from_slice(&b).unwrap();
    let ids: Vec<&str> = v
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["info"]["id"].as_str().unwrap())
        .collect();
    assert_eq!(
        ids,
        ["msg_0007", "msg_0008", "msg_0009", "msg_0010", "msg_0011"]
    );
    // full-history (no limit) MUST stream with no Content-Length — the
    // original OOM-class invariant, now scoped where it actually applies
    // (F9 never memoizes unbounded walks; page_n.is_some() gate).
    let (sf, _bf, hf) = get(&app, "/session/ses_p/message").await;
    assert_eq!(sf, 200);
    assert!(
        hf.get(axum::http::header::CONTENT_LENGTH).is_none(),
        "full-history must stream (no Content-Length) — unbounded OOM class guard"
    );
    let cur = h
        .get("x-next-cursor")
        .expect("cursor header")
        .to_str()
        .unwrap()
        .to_string();
    let link = h
        .get("link")
        .expect("link header")
        .to_str()
        .unwrap()
        .to_string();
    assert!(
        link.contains("?limit=5&before=") && link.ends_with(">; rel=\"next\""),
        "link: {link}"
    );
    assert_eq!(
        h.get("access-control-expose-headers")
            .unwrap()
            .to_str()
            .unwrap(),
        "Link, X-Next-Cursor"
    );

    // page 2: strictly older than the cursor message
    let (s2, b2, h2) = get(
        &app,
        &format!("/session/ses_p/message?limit=5&before={cur}"),
    )
    .await;
    assert_eq!(s2, 200);
    let v2: serde_json::Value = serde_json::from_slice(&b2).unwrap();
    let ids2: Vec<&str> = v2
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["info"]["id"].as_str().unwrap())
        .collect();
    assert_eq!(
        ids2,
        ["msg_0002", "msg_0003", "msg_0004", "msg_0005", "msg_0006"]
    );
    let cur2 = h2
        .get("x-next-cursor")
        .expect("cursor2")
        .to_str()
        .unwrap()
        .to_string();

    // page 3: remaining 2, NO more-cursor headers
    let (s3, b3, h3) = get(
        &app,
        &format!("/session/ses_p/message?limit=5&before={cur2}"),
    )
    .await;
    assert_eq!(s3, 200);
    let v3: serde_json::Value = serde_json::from_slice(&b3).unwrap();
    let ids3: Vec<&str> = v3
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["info"]["id"].as_str().unwrap())
        .collect();
    assert_eq!(ids3, ["msg_0000", "msg_0001"]);
    assert!(h3.get("x-next-cursor").is_none(), "no cursor at end");
    assert!(h3.get("link").is_none());

    // full history: no page headers
    let (s0, b0, h0) = get(&app, "/session/ses_p/message").await;
    assert_eq!(s0, 200);
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&b0)
            .unwrap()
            .as_array()
            .unwrap()
            .len(),
        12
    );
    assert!(h0.get("x-next-cursor").is_none());

    // limit larger than the session: no cursor
    let (s9, _, h9) = get(&app, "/session/ses_p/message?limit=999").await;
    assert_eq!(s9, 200);
    assert!(h9.get("x-next-cursor").is_none());

    // limit=0 → full history (upstream rule), no headers
    let (s0z, b0z, h0z) = get(&app, "/session/ses_p/message?limit=0").await;
    assert_eq!(s0z, 200);
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&b0z)
            .unwrap()
            .as_array()
            .unwrap()
            .len(),
        12
    );
    assert!(h0z.get("x-next-cursor").is_none());
}

#[tokio::test]
async fn limit_query_error_matrix_matches_probes() {
    let (_st, _) = state_with([("fake", "http://127.0.0.1:9")]);
    let dir = std::env::temp_dir().join(format!("ocserve-pg3-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let db = ocserve_store::writer::db_path(&dir);
    let writer = std::sync::Arc::new(ocserve_store::Writer::spawn(db.clone()).unwrap());
    let blobs = std::sync::Arc::new(ocserve_store::BlobStore::new(dir.join("blobs")).unwrap());
    let llm = LlmRegistry {
        limits: std::collections::HashMap::new(),
        endpoints: [("fake".into(), ("http://127.0.0.1:9".into(), String::new()))]
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
            db,
            blobs,
            writer,
            llm,
        },
    );
    seed_session(&st, "ses_m");
    let app = ocserve_http::router(st.clone());

    // probe byte-exact envelopes
    let cases = [
        (
            "/session/ses_m/message?limit=abc",
            r#"{"name":"BadRequest","data":{"message":"Expected an integer, got NaN\n  at [\"limit\"]","kind":"Query"}}"#,
        ),
        (
            "/session/ses_m/message?limit=5.5",
            r#"{"name":"BadRequest","data":{"message":"Expected an integer, got 5.5\n  at [\"limit\"]","kind":"Query"}}"#,
        ),
        (
            "/session/ses_m/message?limit=-1",
            r#"{"name":"BadRequest","data":{"message":"Expected a value greater than or equal to 0, got -1\n  at [\"limit\"]","kind":"Query"}}"#,
        ),
        (
            "/session/ses_m/message?limit=999999999999999999999",
            r#"{"name":"BadRequest","data":{"message":"Expected an integer, got 1e+21\n  at [\"limit\"]","kind":"Query"}}"#,
        ),
        (
            "/session/ses_m/message?before=x",
            r#"{"_tag":"BadRequest"}"#,
        ),
        (
            "/session/ses_m/message?limit=5&before=garbage",
            r#"{"_tag":"BadRequest"}"#,
        ),
    ];
    for (uri, expected) in cases {
        let (s, b, _) = get(&app, uri).await;
        assert_eq!(s, 400, "{uri}");
        assert_eq!(std::str::from_utf8(&b).unwrap(), expected, "{uri}");
    }
    // 404 AFTER query rules (upstream order)
    let (s, b, _) = get(&app, "/session/ses_nope/message?limit=abc").await;
    assert_eq!(s, 400, "query rules precede session lookup");
    assert!(std::str::from_utf8(&b).unwrap().contains("kind"));
    let (s, b, _) = get(&app, "/session/ses_nope/message?limit=5").await;
    assert_eq!(s, 404);
    assert!(
        std::str::from_utf8(&b)
            .unwrap()
            .contains("Session not found")
    );
}

#[tokio::test]
async fn status_reports_busy_then_drains_to_empty() {
    let (st, app) = state_with([("fake", "http://127.0.0.1:9")]);
    seed_session(&st, "ses_b");

    // idle → byte-golden {} (keeps session_status.body green)
    let (s, b, _) = get(&app, "/session/status").await;
    assert_eq!(s, 200);
    assert_eq!(std::str::from_utf8(&b).unwrap(), "{}");

    // held prompt lock → busy entry (value shape = status_busy.body)
    let arc = {
        let mut m = st.prompt_locks.lock();
        m.entry("ses_b".to_string())
            .or_insert_with(|| std::sync::Arc::new(tokio::sync::Mutex::new(())))
            .clone()
    };
    let holder = tokio::spawn({
        let a = arc.clone();
        async move {
            let _g = a.lock().await;
            tokio::time::sleep(std::time::Duration::from_secs(60)).await;
        }
    });
    tokio::time::sleep(std::time::Duration::from_millis(30)).await;
    let (s, b, _) = get(&app, "/session/status").await;
    assert_eq!(s, 200);
    let v: serde_json::Value = serde_json::from_slice(&b).unwrap();
    assert_eq!(v["ses_b"], serde_json::json!({"type": "busy"}));

    // released → {} again (idle entries never listed)
    holder.abort();
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    let (s, b, _) = get(&app, "/session/status").await;
    assert_eq!(s, 200);
    assert_eq!(std::str::from_utf8(&b).unwrap(), "{}");
    // cleanup: remove the test's lock entry
    st.prompt_locks.lock().remove("ses_b");
}

#[tokio::test]
async fn abort_releases_prompt_locks_and_tasks() {
    // Silent provider: accepts the TCP connection, never answers → prompt
    // hangs deterministically (the A3 hazard), then abort must clear every
    // transient map (A2 — LockRelease drops with the aborted task).
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    std::thread::spawn(move || {
        let mut held = Vec::new();
        for s in listener.incoming().flatten() {
            held.push(s); // keep open, never respond
        }
    });
    let hang = format!("http://{addr}");

    let dir = std::env::temp_dir().join(format!("ocserve-abort-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let db = ocserve_store::writer::db_path(&dir);
    let writer = std::sync::Arc::new(ocserve_store::Writer::spawn(db.clone()).unwrap());
    let blobs = std::sync::Arc::new(ocserve_store::BlobStore::new(dir.join("blobs")).unwrap());
    let llm = LlmRegistry {
        limits: std::collections::HashMap::new(),
        endpoints: [("fake".into(), (hang, String::new()))]
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
            db,
            blobs,
            writer,
            llm,
        },
    );
    seed_session(&st, "ses_hang");
    let app = ocserve_http::router(st.clone());

    let req = Request::builder()
        .method("POST")
        .uri("/session/ses_hang/prompt_async")
        .header("content-type", "application/json")
        .body(Body::from(
            r#"{"messageID":"msg_hang0000000000000000000001",
                "parts":[{"type":"text","text":"hang"}],
                "model":{"providerID":"fake","modelID":"m"},"agent":"build"}"#,
        ))
        .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);

    // task registered and running (stream hanging on the silent socket)
    let mut registered = false;
    for _ in 0..100 {
        if !st.prompt_tasks.lock().is_empty() {
            registered = true;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    assert!(registered, "background prompt task must be registered");

    // abort → maps drain (LockRelease is the only cleanup that runs on abort)
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/session/ses_hang/abort")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert!(resp.status().is_success(), "abort: {}", resp.status());
    let mut clean = false;
    for _ in 0..150 {
        let tasks_empty = st.prompt_tasks.lock().is_empty();
        let locks_empty = st.prompt_locks.lock().is_empty();
        if tasks_empty && locks_empty {
            clean = true;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    assert!(clean, "abort must clear prompt_tasks AND prompt_locks (A2)");
    let (s, b, _) = get(&app, "/session/status").await;
    assert_eq!(s, 200);
    assert_eq!(std::str::from_utf8(&b).unwrap(), "{}");
}
