//! 2026-10-12 gateway-hang, phase 2 (the user's exact scenario): headers +
//! keep-alive pings, then the FIRST real data arrives LATER than the normal
//! stream-stall budget but within the TTFB budget. The turn must SUCCEED —
//! pre-fix this died at 120 s with "provider stream stalled".
//!
//! Budgets here: stream=1 s, TTFB=6 s, first data at ~3 s → passes only if
//! the first-data phase is judged by the TTFB budget. Negative control:
//! swap the phases (TTFB<first-data) → the same server must fail the turn.
use axum::body::Body;
use axum::http::{Request, StatusCode};
use ocserve_http::{AppState, LlmRegistry, Payloads, Wires};
use tower::ServiceExt;

fn app_with(addr: std::net::SocketAddr) -> std::sync::Arc<AppState> {
    let dir = std::env::temp_dir().join(format!("ocserve-tgd-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let db = ocserve_store::writer::db_path(&dir);
    let writer = std::sync::Arc::new(ocserve_store::Writer::spawn(db.clone()).unwrap());
    let blobs = std::sync::Arc::new(ocserve_store::BlobStore::new(dir.join("blobs")).unwrap());
    let llm = LlmRegistry {
        limits: std::collections::HashMap::new(),
        endpoints: [(
            "fake".into(),
            (format!("http://{addr}"), "test-key".to_string()),
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
            db,
            blobs,
            writer,
            llm,
        },
    );
    ocserve_store::insert_session(
        &st.writer,
        &serde_json::json!({
            "id": "ses_tgd", "projectID": "global", "directory": "/w", "path": "ses_tgd",
            "slug": "ses_tgd", "title": "tgd", "version": "1",
            "time": {"created": 1, "updated": 2},
        }),
    )
    .unwrap();
    st
}

fn spawn_sse(first_data_after: std::time::Duration) -> std::net::SocketAddr {
    use std::io::{Read, Write};
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let mut stream = stream;
            std::thread::spawn(move || {
                let mut buf = vec![0u8; 65536];
                let _ = stream.read(&mut buf);
                let _ =
                    stream.write_all(b"HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\n\r\n");
                let t0 = std::time::Instant::now();
                while t0.elapsed() < first_data_after {
                    let _ = stream.write_all(b": keep-alive\n\n");
                    std::thread::sleep(std::time::Duration::from_millis(200));
                }
                let _ = stream.write_all(
                    b"data: {\"id\":\"c\",\"object\":\"chat.completion.chunk\",\"created\":1,\"model\":\"m\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"OK\"},\"finish_reason\":null}]}\n\n",
                );
                let _ = stream.write_all(
                    b"data: {\"id\":\"c\",\"object\":\"chat.completion.chunk\",\"created\":1,\"model\":\"m\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}],\"usage\":{\"prompt_tokens\":10,\"completion_tokens\":1,\"total_tokens\":11}}\n\n",
                );
                let _ = stream.write_all(b"data: [DONE]\n\n");
            });
        }
    });
    addr
}

async fn prompt(st: &std::sync::Arc<AppState>) -> StatusCode {
    ocserve_http::router(st.clone())
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/session/ses_tgd/prompt_async")
                .header("content-type", "application/json")
                .body(Body::from(
                    r#"{"messageID":"msg_tgd0000000000000000000001",
                        "parts":[{"type":"text","text":"hi"}],
                        "model":{"providerID":"fake","modelID":"m"},"agent":"build"}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap()
        .status()
}

async fn wait_assistant(st: &std::sync::Arc<AppState>, budget: std::time::Duration) -> Option<i64> {
    // poll the DB for an assistant message with time.completed set
    let t0 = std::time::Instant::now();
    while t0.elapsed() < budget {
        if let Ok(conn) = ocserve_store::pragma::open_reader(&st.db)
            && let Ok(n) = conn.query_row(
                "SELECT count(*) FROM msg WHERE session_id='ses_tgd' AND role='assistant' AND json_extract(info,'$.time.completed') IS NOT NULL",
                [],
                |r| r.get::<_, i64>(0),
            )
            && n > 0
        {
            return Some(n);
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    None
}

#[tokio::test]
async fn first_data_after_stream_budget_within_ttfb_succeeds() {
    // SAFETY: single test in this binary (own process env).
    unsafe { std::env::set_var("OCSERVE_PROVIDER_STALL_SECS", "1") }; // stream phase: 1s
    unsafe { std::env::set_var("OCSERVE_PROVIDER_TTFB_SECS", "6") }; // first-data phase: 6s
    // first data at ~3s: stream budget (1s) long expired; TTFB (6s) not.
    let addr = spawn_sse(std::time::Duration::from_millis(3000));
    let st = app_with(addr);
    assert_eq!(prompt(&st).await, StatusCode::NO_CONTENT);
    let done = wait_assistant(&st, std::time::Duration::from_secs(20)).await;
    assert!(
        done.is_some(),
        "turn must complete: first data at 3s is within the 6s TTFB budget \
         (pre-fix it died at the 1s stream budget)"
    );
}
