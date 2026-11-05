//! `/goal` loop END-TO-END against a real stub provider: the reviewer call and
//! the continuation turn both run for real, and the loop stops exactly where
//! the reference stops.
//!
//! The reviewer asks the model for one line of JSON, so the stub answers a
//! verdict; the continuation is a normal turn. This is the test that proves
//! "reviewer says continue → a real turn runs", which the state-machine tests
//! cannot.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use ocserve_http::{AppState, LlmRegistry, Payloads, Wires};
use serde_json::{Value, json};
use std::io::Read as _;
use std::io::Write as _;
use std::sync::Arc;
use tower::ServiceExt;

/// A provider that answers the REVIEWER with `verdict`, and everything else
/// with a plain answer. Each request gets its own connection.
fn spawn_provider(verdict: &'static str) -> std::net::SocketAddr {
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
                        k.eq_ignore_ascii_case("content-length")
                            .then(|| v.trim().parse::<usize>().ok())
                            .flatten()
                    })
                    .unwrap_or(0);
                while got.len() < head_end + clen {
                    match sock.read(&mut buf) {
                        Ok(0) => break,
                        Ok(k) => got.extend_from_slice(&buf[..k]),
                        Err(_) => break,
                    }
                }
                let body = String::from_utf8_lossy(&got[head_end..]).to_string();
                // The reviewer prompt is the only one that says "reviewer".
                let reply = if body.contains("You are now the reviewer") {
                    verdict
                } else {
                    "working on it"
                };
                let sse = format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\n\
                     cache-control: no-cache\r\nconnection: close\r\n\r\n\
                     data: {{\"choices\":[{{\"delta\":{{\"content\":{}}}}}]}}\n\n\
                     data: {{\"choices\":[{{\"delta\":{{}},\"finish_reason\":\"stop\"}}]}}\n\n\
                     data: {{\"choices\":[],\"usage\":{{\"prompt_tokens\":10,\
                     \"completion_tokens\":2,\"total_tokens\":12}}}}\n\n\
                     data: [DONE]\n\n",
                    serde_json::to_string(reply).unwrap()
                );
                let _ = sock.write_all(sse.as_bytes());
                let _ = sock.flush();
            });
        }
    });
    addr
}

fn setup(addr: std::net::SocketAddr, sid: &str) -> Arc<AppState> {
    let dir = std::env::temp_dir().join(format!("ocserve-goal-{}-{sid}", std::process::id()));
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
            "id": sid, "projectID": "global", "directory": "/w",
            "path": sid, "slug": sid, "title": "t", "version": "1",
            "time": {"created": 1, "updated": 2},
        }),
    )
    .unwrap();
    st
}

async fn req(
    app: &axum::Router,
    method: &str,
    uri: &str,
    body: Option<Value>,
) -> axum::response::Response {
    let mut b = Request::builder().method(method).uri(uri);
    let body = match body {
        Some(v) => {
            b = b.header("content-type", "application/json");
            Body::from(serde_json::to_vec(&v).unwrap())
        }
        None => Body::empty(),
    };
    app.clone().oneshot(b.body(body).unwrap()).await.unwrap()
}

async fn json(r: axum::response::Response) -> Value {
    let bytes = r.into_body().collect().await.unwrap().to_bytes();
    serde_json::from_slice(&bytes).unwrap_or(Value::Null)
}

fn router(st: &Arc<AppState>) -> axum::Router {
    ocserve_http::router(st.clone())
}

/// Poll the goal until it leaves `active` (the loop is a spawned tabk).
async fn wait_goal(st: &Arc<AppState>, sid: &str, want: &str, secs: u64) -> Value {
    for _ in 0..(secs * 10) {
        if let Ok(Some(g)) = ocserve_store::get_goal(&st.db, sid)
            && g.status == want
        {
            return g.to_json();
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    panic!(
        "goal never reached {want}: {:?}",
        ocserve_store::get_goal(&st.db, sid).ok().flatten()
    );
}

/// The reviewer says DONE: the loop must complete the goal, and it must not
/// send a continuation.
#[tokio::test(flavor = "multi_thread")]
async fn reviewer_done_completes_the_goal() {
    let addr = spawn_provider(r#"{"verdict":"done","reason":"the tests pass"}"#);
    let st = setup(addr, "ses_goaldone");
    let a = router(&st);
    let _ = req(
        &a,
        "POST",
        "/session/ses_goaldone/command",
        Some(json!({"command": "goal", "arguments": "make the tests pass"})),
    )
    .await;
    let g = wait_goal(&st, "ses_goaldone", "complete", 20).await;
    assert_eq!(g["status"], "complete");
    assert_eq!(g["reason"], "the tests pass");
    // The kickoff turn ran (the stub answered), so there is an assistant
    // message in the session beyond the synthetic kickoff.
    let msgs = ocserve_store::load_messages(&st.db, "ses_goaldone", None).unwrap();
    assert!(
        msgs.iter().any(|(i, _)| i["role"] == "assistant"),
        "the kickoff turn must have run"
    );
}

/// The reviewer says CONTINUE once, then DONE: exactly one continuation turn
/// runs and `rounds` advances by one.
#[tokio::test(flavor = "multi_thread")]
async fn reviewer_continue_runs_a_real_turn_then_done() {
    // A provider that answers "continue" the first time it is asked and
    // "done" afterwards, so the loop takes exactly one extra round.
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let asked = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let asked2 = asked.clone();
    std::thread::spawn(move || {
        for sock in listener.incoming().flatten() {
            let asked = asked2.clone();
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
                        k.eq_ignore_ascii_case("content-length")
                            .then(|| v.trim().parse::<usize>().ok())
                            .flatten()
                    })
                    .unwrap_or(0);
                while got.len() < head_end + clen {
                    match sock.read(&mut buf) {
                        Ok(0) => break,
                        Ok(k) => got.extend_from_slice(&buf[..k]),
                        Err(_) => break,
                    }
                }
                let body = String::from_utf8_lossy(&got[head_end..]).to_string();
                let reply = if body.contains("You are now the reviewer") {
                    let n = asked.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    if n == 0 {
                        r#"{"verdict":"continue","reason":"tests not run","next_step":"run them"}"#
                    } else {
                        r#"{"verdict":"done","reason":"tests pass now"}"#
                    }
                } else {
                    "working on it"
                };
                let sse = format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\n\
                     cache-control: no-cache\r\nconnection: close\r\n\r\n\
                     data: {{\"choices\":[{{\"delta\":{{\"content\":{}}}}}]}}\n\n\
                     data: {{\"choices\":[{{\"delta\":{{}},\"finish_reason\":\"stop\"}}]}}\n\n\
                     data: {{\"choices\":[],\"usage\":{{\"prompt_tokens\":10,\
                     \"completion_tokens\":2,\"total_tokens\":12}}}}\n\n\
                     data: [DONE]\n\n",
                    serde_json::to_string(reply).unwrap()
                );
                let _ = sock.write_all(sse.as_bytes());
                let _ = sock.flush();
            });
        }
    });

    let st = setup(addr, "ses_goalcont");
    let a = router(&st);
    let _ = req(
        &a,
        "POST",
        "/session/ses_goalcont/command",
        Some(json!({"command": "goal", "arguments": "get the tests green"})),
    )
    .await;
    let g = wait_goal(&st, "ses_goalcont", "complete", 25).await;
    assert_eq!(g["status"], "complete");
    assert_eq!(g["reason"], "tests pass now");
    assert!(
        g["rounds"].as_i64().unwrap() >= 1,
        "one continuation ran: {g}"
    );
    assert_eq!(asked.load(std::sync::atomic::Ordering::SeqCst), 2);
    // The continuation message is in the session, synthetic and goal-tagged.
    let msgs = ocserve_store::load_messages(&st.db, "ses_goalcont", None).unwrap();
    let cont = msgs.iter().find(|(i, parts)| {
        i["role"].as_str() == Some("user")
            && parts
                .iter()
                .any(|p| p["metadata"]["source"].as_str() == Some("goal"))
    });
    assert!(cont.is_some(), "a goal continuation message must exist");
}

/// The reviewer says BLOCKED: the loop pauses with the reviewer's reason.
#[tokio::test(flavor = "multi_thread")]
async fn reviewer_blocked_pauses() {
    let addr = spawn_provider(r#"{"verdict":"blocked","reason":"needs a credential"}"#);
    let st = setup(addr, "ses_goalblocked");
    let a = router(&st);
    let _ = req(
        &a,
        "POST",
        "/session/ses_goalblocked/command",
        Some(json!({"command": "goal", "arguments": "deploy"})),
    )
    .await;
    let g = wait_goal(&st, "ses_goalblocked", "paused", 20).await;
    assert_eq!(g["reason"], "needs a credential");
}

/// An unparsable verdict pauses with the reference's own message rather than
/// leaving the goal active with nothing driving it.
#[tokio::test(flavor = "multi_thread")]
async fn garbage_verdict_pauses() {
    let addr = spawn_provider("I have no idea what to say");
    let st = setup(addr, "ses_goalgarbage");
    let a = router(&st);
    let _ = req(
        &a,
        "POST",
        "/session/ses_goalgarbage/command",
        Some(json!({"command": "goal", "arguments": "do something"})),
    )
    .await;
    let g = wait_goal(&st, "ses_goalgarbage", "paused", 20).await;
    assert_eq!(g["reason"], "The goal reviewer gave no usable verdict");
}

/// A goal whose boot id belongs to another process is paused at boot, never
/// resumed (upstream's boot scan).
#[tokio::test(flavor = "multi_thread")]
async fn goal_from_an_earlier_boot_is_paused() {
    let addr = spawn_provider(r#"{"verdict":"done","reason":"x"}"#);
    let st = setup(addr, "ses_goalboot");
    ocserve_store::set_goal(
        &st.writer,
        "ses_goalboot",
        Some(&ocserve_store::GoalRow {
            id: "goal_old".into(),
            objective: "from a previous run".into(),
            status: "active".into(),
            reason: None,
            rounds: 3,
            max_rounds: 50,
            stalls: None,
            boot: "boot-from-another-process".into(),
            updated_at: 1,
        }),
    )
    .unwrap();
    ocserve_http::goal_loop::pause_goals_from_earlier_boots(&st).await;
    let g = ocserve_store::get_goal(&st.db, "ses_goalboot")
        .unwrap()
        .unwrap();
    assert_eq!(g.status, "paused");
    assert_eq!(
        g.reason.as_deref(),
        Some("The server restarted while the goal was active")
    );
}

/// With the switch off the command is not advertised and not handled: the
/// route falls through to the ordinary "Command not found" path and no state
/// is written. (Discovery is covered by `goal_command_tests` in the runtime;
/// this is the HTTP half.)
#[tokio::test(flavor = "multi_thread")]
async fn unknown_command_falls_through_without_writing_goal_state() {
    let addr = spawn_provider(r#"{"verdict":"done","reason":"x"}"#);
    let st = setup(addr, "ses_goalunk");
    let a = router(&st);
    // A command that does not exist at all: the handler must 400 BEFORE any
    // goal logic, which is where `goal` lands when the switch is off.
    let r = req(
        &a,
        "POST",
        "/session/ses_goalunk/command",
        Some(json!({"command": "definitely-not-a-command", "arguments": "x"})),
    )
    .await;
    assert_eq!(r.status(), StatusCode::BAD_REQUEST);
    assert!(
        json(r).await["data"]["message"]
            .as_str()
            .unwrap()
            .contains("Command not found"),
    );
    assert!(
        ocserve_store::get_goal(&st.db, "ses_goalunk")
            .unwrap()
            .is_none()
    );
}
