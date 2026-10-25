//! Live regression (2026-10-25T17:26:58): a spent opencode free allowance
//! surfaced as a durable `UnknownError` carrying the clean upsell TEXT while
//! the prompt path's typed `APIError` was live-only. Two problems: the client
//! got the wrong variant (its retry/upsell action needs `APIError`), and the
//! durable log disagreed with what was shown.
//!
//! This drives the real prompt path against a provider that answers 429
//! `FreeUsageLimitError`, then asserts the ONE durable `session.error` is the
//! typed, non-retryable APIError with upstream's message.
use axum::body::Body;
use axum::http::{Request, StatusCode};
use ocserve_http::{AppState, LlmRegistry, Payloads, Wires};
use tower::ServiceExt;

fn app_with(addr: std::net::SocketAddr) -> std::sync::Arc<AppState> {
    let dir = std::env::temp_dir().join(format!("ocserve-fte-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let db = ocserve_store::writer::db_path(&dir);
    let writer = std::sync::Arc::new(ocserve_store::Writer::spawn(db.clone()).unwrap());
    let blobs = std::sync::Arc::new(ocserve_store::BlobStore::new(dir.join("blobs")).unwrap());
    let llm = LlmRegistry {
        limits: std::collections::HashMap::new(),
        endpoints: [(
            "opencode".into(),
            (format!("http://{addr}"), "public".to_string()),
        )]
        .into_iter()
        .collect(),
        pricing: Default::default(),
        default_model: ("opencode".into(), "m".into()),
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
    ocserve_store::insert_session(
        &st.writer,
        &serde_json::json!({
            "id": "ses_ft", "projectID": "global", "directory": "/w", "path": "ses_ft",
            "slug": "ses_ft", "title": "ft", "version": "1",
            "time": {"created": 1, "updated": 2},
        }),
    )
    .unwrap();
    st
}

/// 429 with the exact body the free tier returned live.
fn spawn_limited_provider() -> std::net::SocketAddr {
    use std::io::{Read, Write};
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let mut stream = stream;
            std::thread::spawn(move || {
                let mut buf = vec![0u8; 65536];
                let _ = stream.read(&mut buf);
                let body = r#"{"type":"error","error":{"type":"FreeUsageLimitError","message":"Free usage limit reached. Free models work in OpenCode (https://opencode.ai); subscribe to OpenCode Go to keep going: https://opencode.ai/go"}}"#;
                let head = format!(
                    "HTTP/1.1 429 Too Many Requests\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\r\n",
                    body.len()
                );
                let _ = stream.write_all(head.as_bytes());
                let _ = stream.write_all(body.as_bytes());
            });
        }
    });
    addr
}

fn error_events(st: &AppState) -> Vec<serde_json::Value> {
    let conn = ocserve_store::pragma::open_reader(&st.db).unwrap();
    let mut stmt = conn
        .prepare("SELECT payload FROM event WHERE session_id='ses_ft' AND type='session.error@v1' ORDER BY seq")
        .unwrap();
    stmt.query_map([], |r| r.get::<_, String>(0))
        .unwrap()
        .filter_map(|r| r.ok())
        .filter_map(|s| serde_json::from_str(&s).ok())
        .collect()
}

#[tokio::test]
async fn spent_free_allowance_is_one_typed_non_retryable_error() {
    let addr = spawn_limited_provider();
    let st = app_with(addr);
    let status = ocserve_http::router(st.clone())
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/session/ses_ft/prompt_async")
                .header("content-type", "application/json")
                .body(Body::from(
                    r#"{"messageID":"msg_ft0000000000000000000001",
                        "parts":[{"type":"text","text":"hi"}],
                        "model":{"providerID":"opencode","modelID":"m"},"agent":"build"}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap()
        .status();
    assert_eq!(status, StatusCode::NO_CONTENT);

    // Wait for the durable error only: the prompt task handle can outlive the
    // event, and asserting on a timer would waste 20 s on every run.
    let t0 = std::time::Instant::now();
    while t0.elapsed() < std::time::Duration::from_secs(20) && error_events(&st).is_empty() {
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }

    let errs = error_events(&st);
    assert_eq!(
        errs.len(),
        1,
        "exactly one durable error event, got {errs:#?}"
    );
    let e = &errs[0];
    assert_eq!(
        e.pointer("/error/name").and_then(|v| v.as_str()),
        Some("APIError"),
        "the durable event must carry the typed variant the client branches on: {e}"
    );
    assert_eq!(
        e.pointer("/error/data/isRetryable")
            .and_then(|v| v.as_bool()),
        Some(false),
        "a spent allowance is terminal"
    );
    assert_eq!(
        e.pointer("/error/data/statusCode").and_then(|v| v.as_i64()),
        Some(429)
    );
    assert_eq!(
        e.pointer("/error/data/message").and_then(|v| v.as_str()),
        Some("Free usage exceeded, subscribe to Go"),
        "upstream's exact upsell text, not the raw provider body"
    );
}
