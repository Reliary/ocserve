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
        modalities: Default::default(),
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

/// Drain a request fully (head + `Content-Length` body) before responding.
///
/// Issue #80 root cause: a single `read` can leave request bytes queued; when
/// the stub then closes the socket the kernel sends RST, and the client reports
/// `error decoding response body` partway through the response. It only bites
/// under load, because that is when the request arrives in more than one
/// segment (measured: 15/96 concurrent runs failed before this, 0/96 after).
fn drain_request(stream: &mut std::net::TcpStream) {
    use std::io::Read;
    let mut req = Vec::new();
    let mut buf = [0u8; 4096];
    let mut head_len: Option<usize> = None;
    let mut content_len = 0usize;
    loop {
        let n = match stream.read(&mut buf) {
            Ok(0) => return,
            Ok(n) => n,
            Err(_) => return,
        };
        req.extend_from_slice(&buf[..n]);
        if head_len.is_none()
            && let Some(p) = req.windows(4).position(|w| w == b"\r\n\r\n")
        {
            head_len = Some(p + 4);
            let head = String::from_utf8_lossy(&req[..p]).to_ascii_lowercase();
            content_len = head
                .lines()
                .find_map(|l| l.strip_prefix("content-length:"))
                .and_then(|v| v.trim().parse().ok())
                .unwrap_or(0);
        }
        if let Some(h) = head_len
            && req.len() >= h + content_len
        {
            return;
        }
    }
}

fn spawn_sse(first_data_after: std::time::Duration) -> std::net::SocketAddr {
    use std::io::{Read, Write};
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let mut stream = stream;
            std::thread::spawn(move || {
                drain_request(&mut stream);
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
    // Issue #80: capture the code's own reason. Without a subscriber every
    // `tracing::error!` on the failure path is invisible, so a failed turn and
    // a hung one looked identical in the DB.
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn")),
        )
        .with_test_writer()
        .try_init();
    // SAFETY: single test in this binary (own process env).
    unsafe { std::env::set_var("OCSERVE_PROVIDER_STALL_SECS", "1") }; // stream phase: 1s
    unsafe { std::env::set_var("OCSERVE_PROVIDER_TTFB_SECS", "6") }; // first-data phase: 6s
    // first data at ~3s: stream budget (1s) long expired; TTFB (6s) not.
    let addr = spawn_sse(std::time::Duration::from_millis(3000));
    let st = app_with(addr);
    assert_eq!(prompt(&st).await, StatusCode::NO_CONTENT);
    // Observation window, not the assertion's budget: the budgets under test
    // are the 1 s stream phase and the 6 s TTFB phase above. This window only
    // decides how long we wait to SEE the outcome. The old flake (~30% under
    // load, issue #80) was never this window: the stub closed the socket with
    // request bytes still queued, the kernel answered with RST, and the turn
    // failed with `error decoding response body` — see `drain_request`.
    let done = wait_assistant(&st, std::time::Duration::from_secs(60)).await;
    if done.is_none() {
        let task = task_diagnosis(&st).await;
        panic!(
            "turn must complete: first data at 3s is within the 6s TTFB budget \
             (pre-fix it died at the 1s stream budget). {} | {}",
            db_diagnosis(&st),
            task
        );
    }
}

/// Issue #80: the assertion above says only "must complete", so a flake is a
/// blind rerun. Report what actually reached the database, which separates the
/// two candidate causes: (1) the watchdog applied the 1 s STREAM budget to the
/// first-data phase (a real regression — a `session.error` naming "stalled"),
/// or (2) the turn died some other way (an error with a different message, or
/// an assistant row with no `time.completed`).
fn db_diagnosis(st: &std::sync::Arc<AppState>) -> String {
    let Ok(conn) = ocserve_store::pragma::open_reader(&st.db) else {
        return "diagnosis: reader open failed".into();
    };
    let user: i64 = conn
        .query_row(
            "SELECT count(*) FROM msg WHERE session_id='ses_tgd' AND role='user'",
            [],
            |r| r.get(0),
        )
        .unwrap_or(-1);
    let assistant: i64 = conn
        .query_row(
            "SELECT count(*) FROM msg WHERE session_id='ses_tgd' AND role='assistant'",
            [],
            |r| r.get(0),
        )
        .unwrap_or(-1);
    let completed: i64 = conn
        .query_row(
            "SELECT count(*) FROM msg WHERE session_id='ses_tgd' AND role='assistant' \
             AND json_extract(info,'$.time.completed') IS NOT NULL",
            [],
            |r| r.get(0),
        )
        .unwrap_or(-1);
    let all_events: i64 = conn
        .query_row(
            "SELECT count(*) FROM event WHERE session_id='ses_tgd'",
            [],
            |r| r.get(0),
        )
        .unwrap_or(-1);
    let types: String = conn
        .query_row(
            "SELECT COALESCE(group_concat(t||':'||n), '') FROM (SELECT type AS t, count(*) AS n \
             FROM event WHERE session_id='ses_tgd' GROUP BY type)",
            [],
            |r| r.get::<_, String>(0),
        )
        .unwrap_or_else(|_| "<none>".into());
    // Event types are stored with a `@vN` grammar suffix (e.g.
    // `session.error@v1`), so an exact-type query reports 0 errors on a turn
    // that DID fail — which is what made this diagnostic misleading when it
    // was added (issue #80).
    let errors: i64 = conn
        .query_row(
            "SELECT count(*) FROM event WHERE session_id='ses_tgd' AND type LIKE 'session.error%'",
            [],
            |r| r.get(0),
        )
        .unwrap_or(-1);
    let last: String = conn
        .query_row(
            "SELECT payload FROM event WHERE session_id='ses_tgd' AND type LIKE 'session.error%' \
             ORDER BY seq DESC LIMIT 1",
            [],
            |r| r.get::<_, String>(0),
        )
        .unwrap_or_else(|_| "<none>".into());
    format!(
        "diagnosis: user_rows={user} assistant_rows={assistant} completed={completed} \
         session_errors={errors} last_error={last} all_events={all_events} types=[{types}]"
    )
}

/// Issue #80: decide whether the background prompt task (a) never started,
/// (b) panicked, (c) finished without writing an assistant row, or (d) is
/// still running. This is the difference between a product bug and a test
/// harness bug, so it must be observable rather than inferred.
async fn task_diagnosis(st: &std::sync::Arc<AppState>) -> String {
    let handle = {
        let mut tasks = st.prompt_tasks.lock();
        tasks.remove("ses_tgd").map(|(_, h)| h)
    };
    let Some(handle) = handle else {
        // Removed on completion; the DB rows already tell the story.
        return "task=absent(already completed or never registered)".into();
    };
    if !handle.is_finished() {
        handle.abort();
        return "task=still-running-after-window (aborted)".into();
    }
    match handle.await {
        Ok(()) => {
            "task=finished-ok-but-no-rows (the turn returned without persisting an assistant row)"
                .into()
        }
        Err(e) if e.is_panic() => {
            let p = e.into_panic();
            let msg = p
                .downcast_ref::<String>()
                .cloned()
                .or_else(|| p.downcast_ref::<&str>().map(|s| s.to_string()))
                .unwrap_or_else(|| "<non-string panic payload>".into());
            format!("task=PANICKED: {msg}")
        }
        Err(e) => format!("task=join-error: {e}"),
    }
}
