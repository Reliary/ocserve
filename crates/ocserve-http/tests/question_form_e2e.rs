//! v2 question bridge: the `question` tool must ask through the frozen form
//! surface (`form.created {form: Form.Info}`), because v2 clients never parse
//! `question.asked`. CONTROL: the same turn with `OCSERVE_V2` off keeps the v1
//! question event and leaves the form surface empty.
use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use ocserve_http::{AppState, LlmRegistry, Payloads, Wires};
use serde_json::{Value, json};
use std::io::Read as _;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use tower::ServiceExt;

fn question_tool_sse() -> &'static str {
    "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\n\
     cache-control: no-cache\r\nconnection: close\r\n\r\n\
     data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\
     \"id\":\"call_qf_1\",\"type\":\"function\",\"function\":{\
     \"name\":\"question\",\"arguments\":\"{\\\"questions\\\":[{\\\"question\\\":\
     \\\"Welche Farbe magst du am liebsten?\\\",\\\"header\\\":\\\"Testfrage\\\",\
     \\\"options\\\":[{\\\"label\\\":\\\"Blau\\\",\\\"description\\\":\\\"Beruhigend\\\"},\
     {\\\"label\\\":\\\"Grün\\\"}]}]}\"}}]}}]}\n\n\
     data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"tool_calls\"}]}\n\n\
     data: [DONE]\n\n"
}

fn final_sse() -> &'static str {
    "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\n\
     cache-control: no-cache\r\nconnection: close\r\n\r\n\
     data: {\"choices\":[{\"delta\":{\"content\":\"done\"}}]}\n\n\
     data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n\
     data: {\"choices\":[],\"usage\":{\"prompt_tokens\":100,\
     \"completion_tokens\":5,\"total_tokens\":105}}\n\n\
     data: [DONE]\n\n"
}

fn spawn_provider(n: Arc<AtomicUsize>) -> std::net::SocketAddr {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    std::thread::spawn(move || {
        for sock in listener.incoming().flatten() {
            let n = n.clone();
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
                        Ok(k) => got.extend_from_slice(&buf[..k]),
                        Err(_) => break,
                    }
                }
                let seq = n.fetch_add(1, Ordering::SeqCst);
                let body = if seq == 0 {
                    question_tool_sse()
                } else {
                    final_sse()
                };
                let _ = std::io::Write::write_all(&mut sock, body.as_bytes());
                let _ = std::io::Write::flush(&mut sock);
            });
        }
    });
    addr
}

fn setup(addr: std::net::SocketAddr, v2: bool) -> Arc<AppState> {
    let dir = std::env::temp_dir().join(format!(
        "ocserve-qf-{}-{}",
        std::process::id(),
        if v2 { "v2" } else { "v1" }
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let db = ocserve_store::writer::db_path(&dir);
    let writer = Arc::new(ocserve_store::Writer::spawn(db.clone()).unwrap());
    let blobs = Arc::new(ocserve_store::BlobStore::new(dir.join("blobs")).unwrap());
    let mut limits = std::collections::HashMap::new();
    limits.insert(
        ("fake".to_string(), "m".to_string()),
        json!({"context": 1_000_000}),
    );
    let llm = LlmRegistry {
        modalities: Default::default(),
        limits,
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
            "id": "ses_qf", "projectID": "global", "directory": "/w",
            "path": "ses_qf", "slug": "ses_qf", "title": "qf", "version": "1",
            "time": {"created": 1, "updated": 2},
        }),
    )
    .unwrap();
    st
}

fn events(st: &AppState, ty: &str) -> Vec<Value> {
    let conn = ocserve_store::pragma::open_reader(&st.db).unwrap();
    let mut stmt = conn
        .prepare("SELECT type, payload FROM event WHERE session_id='ses_qf' ORDER BY seq")
        .unwrap();
    stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))
        .unwrap()
        .filter_map(|r| r.ok())
        .filter(|(t, _)| t == ty || t == &format!("{ty}@v1"))
        .filter_map(|(_, p)| serde_json::from_str::<Value>(&p).ok())
        .collect()
}

async fn wait_for(st: &AppState, ty: &str) -> Value {
    for _ in 0..500 {
        if let Some(v) = events(st, ty).into_iter().next() {
            return v;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    panic!("{ty} never emitted");
}

async fn get(app: &axum::Router, uri: &str) -> (StatusCode, Value) {
    let resp = app
        .clone()
        .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = resp.status();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

async fn post(app: &axum::Router, uri: &str, body: Value) -> StatusCode {
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
        .status()
}

async fn start_prompt(app: &axum::Router) {
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/session/ses_qf/prompt_async")
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({
                        "messageID": "msg_qf0000000000000000000001",
                        "parts": [{"type": "text", "text": "ask me a question"}],
                        "model": {"providerID": "fake", "modelID": "m"},
                        "agent": "build",
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);
}

async fn wait_done(st: &AppState) {
    for _ in 0..1000 {
        if st.prompt_tasks.lock().is_empty() && st.prompt_locks.lock().is_empty() {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    panic!("prompt did not complete");
}

fn tool_output(st: &AppState) -> String {
    let conn = ocserve_store::pragma::open_reader(&st.db).unwrap();
    conn.query_row(
        "SELECT inline FROM msg_part \
         WHERE session_id='ses_qf' AND type='tool' AND inline IS NOT NULL LIMIT 1",
        [],
        |r| r.get::<_, String>(0),
    )
    .unwrap()
}

fn pending_form_rows(st: &AppState) -> i64 {
    let conn = ocserve_store::pragma::open_reader(&st.db).unwrap();
    conn.query_row("SELECT count(*) FROM form", [], |r| r.get(0))
        .unwrap()
}

#[tokio::test]
async fn v2_question_tool_asks_through_the_form_surface() {
    let addr = spawn_provider(Arc::new(AtomicUsize::new(0)));
    let st = setup(addr, true);
    let app = ocserve_http::router(st.clone());
    start_prompt(&app).await;

    let created = wait_for(&st, "form.created").await;
    let form = &created["form"];
    let fid = form["id"].as_str().expect("form id").to_string();
    assert!(fid.starts_with("frm_"), "frozen brand: {created}");
    assert_eq!(form["sessionID"], "ses_qf");
    assert_eq!(form["title"], "Testfrage");
    assert!(
        created.get("id").is_none(),
        "CONTROL: the old flat envelope must stay gone: {created}"
    );
    assert_eq!(form["fields"][0]["key"], "q0");
    assert_eq!(form["fields"][0]["type"], "string");
    assert_eq!(form["fields"][0]["custom"], true);
    assert_eq!(form["fields"][0]["options"][0]["value"], "Blau");
    assert_eq!(form["fields"][0]["options"][0]["label"], "Blau");

    // The surface a v2 client polls sees the same ask.
    let (status, list) = get(&app, "/api/session/ses_qf/form").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(list["data"][0]["id"], fid.as_str());

    let status = post(
        &app,
        &format!("/api/session/ses_qf/form/{fid}/reply"),
        json!({"answer": {"q0": "Blau"}}),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    wait_done(&st).await;

    let replied = wait_for(&st, "form.replied").await;
    assert_eq!(replied["id"], fid.as_str());
    assert_eq!(replied["sessionID"], "ses_qf");
    assert_eq!(replied["answer"]["q0"], "Blau");
    assert_eq!(
        events(&st, "form.replied").len(),
        1,
        "route + bridge must publish exactly one settlement"
    );
    assert!(
        events(&st, "question.asked").is_empty(),
        "v2 must not emit the invisible v1 ask"
    );

    let out = tool_output(&st);
    assert!(
        out.contains("Welche Farbe magst du am liebsten?") && out.contains("Blau"),
        "tool output must carry the answer: {out}"
    );
}

#[tokio::test]
async fn v1_question_tool_keeps_the_v1_surface() {
    // CONTROL for the flag gate: v2 off → no form, v1 ask as before.
    let addr = spawn_provider(Arc::new(AtomicUsize::new(0)));
    let st = setup(addr, false);
    let app = ocserve_http::router(st.clone());
    start_prompt(&app).await;

    let asked = wait_for(&st, "question.asked").await;
    let qid = asked["id"].as_str().expect("question id").to_string();
    assert_eq!(asked["sessionID"], "ses_qf");
    // The v2 routes are dark under v1; assert the durable surface directly.
    assert_eq!(
        pending_form_rows(&st),
        0,
        "v1 must not create a form for the question tool"
    );

    let status = post(
        &app,
        &format!("/question/{qid}/reply"),
        json!({"answers": [["Blau"]]}),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    wait_done(&st).await;
    assert_eq!(events(&st, "question.replied").len(), 1);
    assert!(events(&st, "form.created").is_empty());
}
