//! A3 provider-stall watchdog (own test binary: process-local env).
use axum::body::Body;
use axum::http::{Request, StatusCode};
use ocserve_http::{AppState, LlmRegistry, Payloads, Wires};
use tower::ServiceExt;

#[tokio::test]
async fn silent_provider_fails_within_stall_budget_and_releases_state() {
    // SAFETY: this test binary runs a single #[tokio::test]; no other thread
    // reads env concurrently before this point (own process = own env).
    unsafe { std::env::set_var("OCSERVE_PROVIDER_STALL_SECS", "1") };
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    std::thread::spawn(move || {
        let mut held = Vec::new();
        for s in listener.incoming().flatten() {
            held.push(s);
        }
    });

    let dir = std::env::temp_dir().join(format!("ocserve-stall-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let db = ocserve_store::writer::db_path(&dir);
    let writer = std::sync::Arc::new(ocserve_store::Writer::spawn(db.clone()).unwrap());
    let blobs = std::sync::Arc::new(ocserve_store::BlobStore::new(dir.join("blobs")).unwrap());
    let llm = LlmRegistry {
        limits: std::collections::HashMap::new(),
        endpoints: [("fake".into(), (format!("http://{addr}"), String::new()))]
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
            "id": "ses_stall", "projectID": "global", "directory": "/w", "path": "ses_stall",
            "slug": "ses_stall", "title": "stall", "version": "1",
            "time": {"created": 1, "updated": 2},
        }),
    )
    .unwrap();
    let app = ocserve_http::router(st.clone());

    let t0 = std::time::Instant::now();
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/session/ses_stall/prompt_async")
                .header("content-type", "application/json")
                .body(Body::from(
                    r#"{"messageID":"msg_stall000000000000000000001",
                        "parts":[{"type":"text","text":"hang"}],
                        "model":{"providerID":"fake","modelID":"m"},"agent":"build"}"#,
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);

    // watchdog must fire (1s budget) → task fails → maps drain WITHOUT abort
    let mut clean = false;
    for _ in 0..200 {
        if st.prompt_tasks.lock().is_empty() && st.prompt_locks.lock().is_empty() {
            clean = true;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
    let elapsed = t0.elapsed();
    assert!(clean, "stall watchdog must release prompt state");
    assert!(
        elapsed < std::time::Duration::from_secs(10),
        "should trip at ~1s, took {elapsed:?}"
    );
    // error was logged (prompt_async failure path) — status idle
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/session/status")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let bytes = axum::body::to_bytes(resp.into_body(), 64).await.unwrap();
    assert_eq!(&bytes[..], b"{}", "idle after stall failure");
}
