//! K-EVENT-CONTRACT: a failed tool call must persist the frozen
//! `ToolStateError` shape ({status,input,error,time}) and the model must see
//! that error on the next turn.
//!
//! The 2026-11-17 bug: the terminal state was built once with the *completed*
//! shape (`{status:"error", output, title, metadata}`), which violates the
//! frozen union (`ToolStateError` requires `error`, forbids output/title) on
//! every failed tool call. The live event oracle (rule 19) reported 86 rows.
//! The same shape also meant history rebuild read `state.output` only.
//!
//! Negative control (run during development): restore the single-shape
//! builder -> `failed_call_persists_the_frozen_error_state` fails on the
//! `error` key, and `failed_call_error_reaches_the_next_provider_request`
//! fails on the captured body.
use axum::body::Body;
use axum::http::{Request, StatusCode};
use ocserve_http::{AppState, LlmRegistry, Payloads, Wires};
use serde_json::json;
use std::io::Read as _;
use std::sync::{Arc, Mutex};
use tower::ServiceExt;

const MARKER: &str = "FAILED_XYZMARK";

const TOOL_CALL_SSE: &str = "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\n\
         cache-control: no-cache\r\nconnection: close\r\n\r\n\
         data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\
         \"id\":\"call_err_1\",\"type\":\"function\",\"function\":{\
         \"name\":\"bash\",\"arguments\":\"{\\\"command\\\":\\\"printf 'FAILED_%s\\\\n' XYZMARK; printf 'FAILED_%s\\\\n' XYZMARK >&2; exit 3\\\"}\"\
         }}]}}]}\n\n\
         data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"tool_calls\"}]}\n\n\
         data: [DONE]\n\n";

const FINAL_SSE: &str = "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\n\
     cache-control: no-cache\r\nconnection: close\r\n\r\n\
     data: {\"choices\":[{\"delta\":{\"content\":\"done\"}}]}\n\n\
     data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n\
     data: {\"choices\":[],\"usage\":{\"prompt_tokens\":100,\
     \"completion_tokens\":5,\"total_tokens\":105}}\n\n\
     data: [DONE]\n\n";

/// Records every provider request body; first request asks for the failing
/// tool call, later requests finish with text.
fn spawn_provider() -> (std::net::SocketAddr, Arc<Mutex<Vec<String>>>) {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let bodies: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let bodies2 = bodies.clone();
    std::thread::spawn(move || {
        for sock in listener.incoming().flatten() {
            let bodies = bodies2.clone();
            std::thread::spawn(move || {
                let mut sock = sock;
                let _ = sock.set_read_timeout(Some(std::time::Duration::from_secs(10)));
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
                let seq = {
                    let mut b = bodies.lock().unwrap();
                    b.push(String::from_utf8_lossy(&got[head_end..head_end + clen]).to_string());
                    b.len() - 1
                };
                let body = if seq == 0 { TOOL_CALL_SSE } else { FINAL_SSE };
                let _ = std::io::Write::write_all(&mut sock, body.as_bytes());
                let _ = std::io::Write::flush(&mut sock);
            });
        }
    });
    (addr, bodies)
}

fn setup(addr: std::net::SocketAddr) -> Arc<AppState> {
    // Unique per CALL: the two tests run in parallel in one process, and a
    // pid-only key made them delete each other's database (the same
    // fixture-collision class the repo fixed in six other files).
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!("ocserve-tes-{}-{}", std::process::id(), n));
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
        &json!({
            "id": "ses_tes", "projectID": "global", "directory": "/",
            "path": "", "slug": "ses_tes", "title": "tes", "version": "1",
            "time": {"created": 1, "updated": 2},
        }),
    )
    .unwrap();
    st
}

async fn drive_turn(st: &Arc<AppState>) {
    let app = ocserve_http::router(st.clone());
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/session/ses_tes/prompt_async")
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({
                        "messageID": "msg_tes00000000000000000000001",
                        "parts": [{"type": "text", "text": "run the failing command"}],
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

    // Answer the (default-ask) bash permission with `always`, then wait for
    // the turn to finish.
    let mut answered: Vec<String> = Vec::new();
    for _ in 0..800 {
        let pending: Vec<(String, String)> = {
            let conn = ocserve_store::pragma::open_reader(&st.db).unwrap();
            let asked = conn
                .prepare(
                    "SELECT payload FROM event WHERE session_id='ses_tes' \
                     AND type IN ('permission.asked','permission.asked@v1')",
                )
                .unwrap()
                .query_map([], |r| r.get::<_, String>(0))
                .unwrap()
                .filter_map(|r| r.ok())
                .collect::<Vec<String>>();
            let replied = conn
                .prepare(
                    "SELECT json_extract(payload,'$.requestID') FROM event \
                     WHERE session_id='ses_tes' \
                     AND type IN ('permission.replied','permission.replied@v1')",
                )
                .unwrap()
                .query_map([], |r| r.get::<_, String>(0))
                .unwrap()
                .filter_map(|r| r.ok())
                .collect::<Vec<String>>();
            asked
                .iter()
                .filter_map(|d| serde_json::from_str::<serde_json::Value>(d).ok())
                .filter_map(|v| v["id"].as_str().map(String::from))
                .filter(|id| !replied.contains(id))
                .map(|id| (id.clone(), id))
                .collect()
        };
        for (perm_id, _) in pending {
            let resp = app
                .clone()
                .oneshot(
                    Request::builder()
                        .method("POST")
                        .uri(format!("/permission/{perm_id}/reply"))
                        .header("content-type", "application/json")
                        .body(Body::from(json!({"reply": "always"}).to_string()))
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(resp.status(), StatusCode::OK);
            answered.push(perm_id);
        }
        if st.prompt_tasks.lock().is_empty()
            && st.prompt_locks.lock().is_empty()
            && !answered.is_empty()
        {
            // one extra beat so the post-turn writes land
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
    assert!(
        st.prompt_tasks.lock().is_empty() && st.prompt_locks.lock().is_empty(),
        "turn did not finish"
    );
}

fn failed_tool_part(st: &Arc<AppState>) -> serde_json::Value {
    let conn = ocserve_store::pragma::open_reader(&st.db).unwrap();
    let payloads: Vec<String> = conn
        .prepare(
            "SELECT payload FROM event WHERE session_id='ses_tes' \
             AND type IN ('message.part.updated','message.part.updated@v1') \
             ORDER BY seq DESC LIMIT 200",
        )
        .unwrap()
        .query_map([], |r| r.get::<_, String>(0))
        .unwrap()
        .filter_map(|r| r.ok())
        .collect();
    for p in payloads {
        let v: serde_json::Value = serde_json::from_str(&p).unwrap();
        let part = &v["part"];
        if part["type"] == "tool" && part["tool"] == "bash" {
            return part.clone();
        }
    }
    panic!("no bash tool part was persisted");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn failed_call_persists_the_frozen_error_state() {
    let (addr, _bodies) = spawn_provider();
    let st = setup(addr);
    drive_turn(&st).await;

    let part = failed_tool_part(&st);
    let state = &part["state"];
    assert_eq!(state["status"], "error", "part={part}");
    assert!(
        state["error"].as_str().is_some_and(|s| s.contains(MARKER)),
        "ToolStateError.error must carry the failure text: part={part}"
    );
    // ToolStateError has additionalProperties:false — these keys are illegal.
    assert!(
        state.get("output").is_none(),
        "error state must not carry `output`: part={part}"
    );
    assert!(
        state.get("title").is_none(),
        "error state must not carry `title`: part={part}"
    );
    assert!(state["time"]["start"].is_number());
    assert!(state["time"]["end"].is_number());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn failed_call_error_reaches_the_next_provider_request() {
    let (addr, bodies) = spawn_provider();
    let st = setup(addr);
    drive_turn(&st).await;

    // The in-turn follow-up uses the live output variable, so it cannot prove
    // the HISTORY read. Send a SECOND user turn: its request is rebuilt from
    // stored parts, which is the path that read only `state.output`.
    let before = bodies.lock().unwrap().len();
    let app = ocserve_http::router(st.clone());
    let resp = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/session/ses_tes/prompt_async")
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({
                        "messageID": "msg_tes00000000000000000000002",
                        "parts": [{"type": "text", "text": "what happened?"}],
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
    for _ in 0..800 {
        if st.prompt_tasks.lock().is_empty() && st.prompt_locks.lock().is_empty() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }

    let captured = bodies.lock().unwrap().clone();
    assert!(
        captured.len() > before,
        "expected a new-turn request, got {captured:?}"
    );
    let last = captured.last().unwrap();
    assert!(
        last.contains(MARKER),
        "the failing tool's error text must reach the model on a later turn \
         (history rebuild read only state.output)"
    );
}
