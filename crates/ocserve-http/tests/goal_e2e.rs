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

/// The kickoff gets a normal answer, but the reviewer stays open until its
/// request is cancelled. Deleting the session must abort that tracked review.
fn spawn_provider_holding_reviewer() -> (
    std::net::SocketAddr,
    std::sync::mpsc::Receiver<()>,
    std::sync::mpsc::Receiver<()>,
) {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let (entered_tx, entered_rx) = std::sync::mpsc::channel();
    let (closed_tx, closed_rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        for sock in listener.incoming().flatten() {
            let entered_tx = entered_tx.clone();
            let closed_tx = closed_tx.clone();
            std::thread::spawn(move || {
                let mut sock = sock;
                let _ = sock.set_read_timeout(Some(std::time::Duration::from_secs(10)));
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
                    .find_map(|line| {
                        let (key, value) = line.split_once(':')?;
                        key.eq_ignore_ascii_case("content-length")
                            .then(|| value.trim().parse::<usize>().ok())
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
                if body.contains("You are now the reviewer") {
                    let _ = entered_tx.send(());
                    loop {
                        match sock.read(&mut buf) {
                            Ok(0) => break,
                            Ok(_) => {}
                            Err(_) => break,
                        }
                    }
                    let _ = closed_tx.send(());
                    return;
                }
                let sse = "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\n\
                    cache-control: no-cache\r\nconnection: close\r\n\r\n\
                    data: {\"choices\":[{\"delta\":{\"content\":\"working on it\"}}]}\n\n\
                    data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n\
                    data: {\"choices\":[],\"usage\":{\"prompt_tokens\":10,\"completion_tokens\":2,\"total_tokens\":12}}\n\n\
                    data: [DONE]\n\n";
                let _ = sock.write_all(sse.as_bytes());
                let _ = sock.flush();
            });
        }
    });
    (addr, entered_rx, closed_rx)
}

fn setup(addr: std::net::SocketAddr, sid: &str) -> Arc<AppState> {
    setup_with_goal_cmd(addr, sid, true)
}

/// `advertise_goal=false` mirrors what `build_commands` serves when the kill
/// switch is OFF: the built-in entry is not in the list at all. The fixture
/// must match production, or the switch test measures the fixture.
fn setup_with_goal_cmd(
    addr: std::net::SocketAddr,
    sid: &str,
    advertise_goal: bool,
) -> Arc<AppState> {
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
    // PRODUCTION SHAPE (regression: this fixture used `Payloads::default()`,
    // whose empty command list made `template` None, so the dispatcher's
    // `template.is_none()` test fired here but never in production, where the
    // advertised goal entry carries `template: ""` because the v1 Command
    // schema requires it. The fixture must match what `build_commands` serves.)
    let payloads = Payloads {
        command: if advertise_goal {
            vec![json!({
                "name": "goal",
                "description": "keep working until an objective is met [pause|resume|clear]",
                "source": "command",
                "template": "",
                "hints": [],
            })]
        } else {
            Vec::new()
        },
        ..Payloads::default()
    };
    let st = AppState::with_wiring(
        None,
        payloads,
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

/// THE REGRESSION (2026-11-05, found from a real client): the built-in goal
/// entry is advertised with `template: ""` because the v1 `Command` schema
/// requires the field, and the dispatcher tested `template.is_none()` — true
/// only in the old fixture, never in production. A real `/goal <objective>`
/// therefore fell through to template expansion, produced an EMPTY prompt,
/// and the model call failed.
///
/// This test is the contract the fixture above now enforces: the response is
/// `SessionV1.WithParts` (assistant info + parts), not a goal-state object.
#[tokio::test(flavor = "multi_thread")]
async fn v1_goal_command_returns_with_parts_not_a_goal_state() {
    let addr = spawn_provider(r#"{"verdict":"done","reason":"x"}"#);
    let st = setup(addr, "ses_goalwithparts");
    let a = router(&st);
    let r = req(
        &a,
        "POST",
        "/session/ses_goalwithparts/command",
        Some(json!({"command": "goal", "arguments": "get the tests green"})),
    )
    .await;
    assert_eq!(r.status(), StatusCode::OK);
    let v = json(r).await;
    // A strict client parses `info` as an AssistantMessage.
    assert_eq!(v["info"]["role"], "assistant");
    assert!(
        v["info"]["id"]
            .as_str()
            .is_some_and(|s| s.starts_with("msg_")),
        "info must be a real message: {v}"
    );
    // The kickoff turn ran synchronously: it produced content, not an empty
    // parts array.
    assert!(
        v["parts"].as_array().is_some_and(|p| !p.is_empty()),
        "the kickoff turn must return its parts: {v}"
    );
    assert!(
        v["parts"]
            .as_array()
            .unwrap()
            .iter()
            .any(|p| p["type"] == "text" && p["text"].as_str().is_some_and(|t| !t.is_empty())),
        "the response must carry the turn's text: {v}"
    );
    // The goal is armed and the continuation message is goal-tagged.
    let g = ocserve_store::get_goal(&st.db, "ses_goalwithparts")
        .unwrap()
        .expect("goal armed");
    assert!(g.status == "active" || g.status == "complete");
    let msgs = ocserve_store::load_messages(&st.db, "ses_goalwithparts", None).unwrap();
    assert!(
        msgs.iter().any(|(_, parts)| parts
            .iter()
            .any(|p| p["metadata"]["source"].as_str() == Some("goal"))),
        "the kickoff must be tagged on the PART"
    );
}

/// The negative control for the dispatch predicate: with a NON-EMPTY user
/// template named `goal`, the built-in must NOT fire — the user's command runs
/// as an ordinary template command. (If the predicate regresses to name-only,
/// this test goes red: the built-in would swallow the user's template.)
#[tokio::test(flavor = "multi_thread")]
async fn a_user_goal_template_beats_the_builtin() {
    let addr = spawn_provider(r#"{"verdict":"done","reason":"x"}"#);
    let st = setup(addr, "ses_goaluser");
    // Replace the command list through the SAME store the production reload
    // writes to (there is no dedicated setter; the reload owns this lock).
    st.payloads.write().command = vec![json!({
        "name": "goal",
        "description": "mine",
        "source": "command",
        "template": "USER TEMPLATE $ARGUMENTS",
        "hints": ["$ARGUMENTS"],
    })];
    let a = router(&st);
    let r = req(
        &a,
        "POST",
        "/session/ses_goaluser/command",
        Some(json!({"command": "goal", "arguments": "args"})),
    )
    .await;
    assert_eq!(r.status(), StatusCode::OK);
    // No goal state was written: the user's command won, exactly as upstream's
    // command.transform ordering implies.
    assert!(
        ocserve_store::get_goal(&st.db, "ses_goaluser")
            .unwrap()
            .is_none(),
        "a user template named goal must not arm the built-in loop"
    );
}

/// REGRESSION (2026-11-05 SRE review, HIGH): the reviewer ran against a
/// SYNTHETIC session id (`ses_transient_goal_reviewer`), so the history loader
/// found nothing and the judge was asked to judge a conversation it could not
/// see. It must run against the REAL session so the evidence reaches the wire.
#[tokio::test(flavor = "multi_thread")]
async fn reviewer_sees_the_conversation_it_judges() {
    // A provider that records whether the REVIEWER request carried the unique
    // evidence marker the session history holds.
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let saw_evidence = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let saw2 = saw_evidence.clone();
    std::thread::spawn(move || {
        for sock in listener.incoming().flatten() {
            let saw = saw2.clone();
            std::thread::spawn(move || {
                let mut sock = sock;
                let _ = sock.set_read_timeout(Some(std::time::Duration::from_secs(5)));
                let mut got = Vec::new();
                let mut buf = [0u8; 8192];
                let head_end = loop {
                    match sock.read(&mut buf) {
                        Ok(0) | Err(_) => return,
                        Ok(k) => got.extend_from_slice(&buf[..k]),
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
                        Ok(0) | Err(_) => break,
                        Ok(k) => got.extend_from_slice(&buf[..k]),
                    }
                }
                let body = String::from_utf8_lossy(&got[head_end..]).to_string();
                let is_reviewer = body.contains("You are now the reviewer");
                if is_reviewer && body.contains("EVIDENCE-MARKER-9F3A") {
                    saw.store(true, std::sync::atomic::Ordering::SeqCst);
                }
                let reply = if is_reviewer {
                    r#"{"verdict":"done","reason":"evidence present"}"#
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
            });
        }
    });

    let st = setup(addr, "ses_goalhistory");
    // Put a real user+assistant exchange carrying the marker into the session.
    let now = 1_790_000_000_000i64;
    ocserve_store::insert_message(
        &st.writer,
        None,
        "ses_goalhistory",
        &json!({
            "id": "msg_hist_user", "sessionID": "ses_goalhistory", "role": "user",
            "time": {"created": now}, "summary": {"diffs": []},
        }),
        &[json!({"id": "prt_hist_u", "sessionID": "ses_goalhistory",
                 "messageID": "msg_hist_user", "type": "text",
                 "text": "earlier work EVIDENCE-MARKER-9F3A"})],
    )
    .unwrap();
    ocserve_store::insert_message(
        &st.writer,
        None,
        "ses_goalhistory",
        &json!({
            "id": "msg_hist_ai", "sessionID": "ses_goalhistory", "role": "assistant",
            "time": {"created": now + 1, "completed": now + 2},
            "parentID": "msg_hist_user", "finish": "stop",
            "modelID": "m", "providerID": "fake", "mode": "build", "agent": "build",
            "path": {"cwd": "/w", "root": "/"}, "cost": 0.0,
            "tokens": {"total": 0, "input": 0, "output": 0, "reasoning": 0,
                       "cache": {"write": 0, "read": 0}},
        }),
        &[json!({"id": "prt_hist_a", "sessionID": "ses_goalhistory",
                 "messageID": "msg_hist_ai", "type": "text",
                 "text": "checked: tests pass"})],
    )
    .unwrap();

    let a = router(&st);
    let _ = req(
        &a,
        "POST",
        "/session/ses_goalhistory/command",
        Some(json!({"command": "goal", "arguments": "finish it"})),
    )
    .await;
    let g = wait_goal(&st, "ses_goalhistory", "complete", 20).await;
    assert_eq!(g["reason"], "evidence present");
    assert!(
        saw_evidence.load(std::sync::atomic::Ordering::SeqCst),
        "the reviewer request must carry the session's conversation \
         (the judge ran against a synthetic id and saw nothing)"
    );
}

/// The goal CONTEXT HOOK (upstream goal.ts:219-231): on a USER turn of an
/// active goal whose objective is no longer in view (a compaction dropped the
/// kickoff), a non-persisted reminder must reach the provider. This is what
/// keeps an overnight goal on task after history is summarized.
#[tokio::test(flavor = "multi_thread")]
async fn a_user_turn_on_an_active_goal_carries_the_reminder() {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let saw = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let saw2 = saw.clone();
    std::thread::spawn(move || {
        for sock in listener.incoming().flatten() {
            let saw = saw2.clone();
            std::thread::spawn(move || {
                let mut sock = sock;
                let _ = sock.set_read_timeout(Some(std::time::Duration::from_secs(5)));
                let mut got = Vec::new();
                let mut buf = [0u8; 8192];
                let head_end = loop {
                    match sock.read(&mut buf) {
                        Ok(0) | Err(_) => return,
                        Ok(k) => got.extend_from_slice(&buf[..k]),
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
                        Ok(0) | Err(_) => break,
                        Ok(k) => got.extend_from_slice(&buf[..k]),
                    }
                }
                let body = String::from_utf8_lossy(&got[head_end..]).to_string();
                if body.contains("UNIQUE-GOAL-OBJECTIVE-77")
                    && body.contains("A goal is active for this session")
                {
                    saw.store(true, std::sync::atomic::Ordering::SeqCst);
                }
                let sse = format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\n\
                     cache-control: no-cache\r\nconnection: close\r\n\r\n\
                     data: {{\"choices\":[{{\"delta\":{{\"content\":{}}}}}]}}\n\n\
                     data: {{\"choices\":[{{\"delta\":{{}},\"finish_reason\":\"stop\"}}]}}\n\n\
                     data: {{\"choices\":[],\"usage\":{{\"prompt_tokens\":5,\
                     \"completion_tokens\":1,\"total_tokens\":6}}}}\n\n\
                     data: [DONE]\n\n",
                    serde_json::to_string("ok").unwrap()
                );
                let _ = sock.write_all(sse.as_bytes());
            });
        }
    });
    let st = setup(addr, "ses_goalreminder");
    // Seed the goal with an objective that is NOT in the conversation (the
    // compaction case: the kickoff is gone from the filtered history).
    ocserve_store::set_goal(
        &st.writer,
        "ses_goalreminder",
        Some(&ocserve_store::GoalRow {
            id: "goal_rem".into(),
            objective: "UNIQUE-GOAL-OBJECTIVE-77".into(),
            status: "active".into(),
            reason: None,
            rounds: 0,
            max_rounds: 50,
            stalls: None,
            boot: ocserve_http::goal_loop::boot_id().to_string(),
            updated_at: 1,
        }),
    )
    .unwrap();
    let a = router(&st);
    let r = req(
        &a,
        "POST",
        "/session/ses_goalreminder/message",
        Some(json!({
            "parts": [{"type": "text", "text": "status?"}],
            "model": {"providerID": "fake", "modelID": "m"},
        })),
    )
    .await;
    if !r.status().is_success() {
        let b = r.into_body().collect().await.unwrap().to_bytes();
        panic!("user turn failed: {}", String::from_utf8_lossy(&b));
    }
    assert!(
        saw.load(std::sync::atomic::Ordering::SeqCst),
        "an active goal whose objective is not in view must inject the \
         non-persisted reminder into the provider request"
    );
    // Non-persisted: no reminder part in the stored history.
    let msgs = ocserve_store::load_messages(&st.db, "ses_goalreminder", None).unwrap();
    let stored = serde_json::to_string(&msgs).unwrap();
    assert!(
        !stored.contains("A goal is active for this session"),
        "the reminder must not be persisted"
    );
}

/// REGRESSION (2026-11-05 reviews): aborting a session did not stop the goal
/// loop. `/abort` cancelled the current prompt task, the driver finished its
/// turn, the reviewer said continue, and another turn started. The goal must
/// be PAUSED (upstream pauses on a user interrupt) and no further round may
/// run.
///
/// DETERMINISTIC: the provider holds the kickoff turn open on a gate, so the
/// abort always lands mid-turn (no sleep race), and every wait is bounded so a
/// regression fails instead of hanging the suite.
#[tokio::test(flavor = "multi_thread")]
async fn abort_pauses_the_goal_and_stops_the_loop() {
    use std::sync::{Condvar, Mutex};
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let gate = Arc::new((Mutex::new(false), Condvar::new()));
    let seen = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let reviews = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let g2 = gate.clone();
    let s2 = seen.clone();
    let r2 = reviews.clone();
    std::thread::spawn(move || {
        for sock in listener.incoming().flatten() {
            let g = g2.clone();
            let seen = s2.clone();
            let reviews = r2.clone();
            std::thread::spawn(move || {
                let mut sock = sock;
                let _ = sock.set_read_timeout(Some(std::time::Duration::from_secs(10)));
                let mut got = Vec::new();
                let mut buf = [0u8; 8192];
                let head_end = loop {
                    match sock.read(&mut buf) {
                        Ok(0) | Err(_) => return,
                        Ok(k) => got.extend_from_slice(&buf[..k]),
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
                        Ok(0) | Err(_) => break,
                        Ok(k) => got.extend_from_slice(&buf[..k]),
                    }
                }
                let body = String::from_utf8_lossy(&got[head_end..]).to_string();
                if body.contains("You are now the reviewer") {
                    reviews.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    let sse = format!(
                        "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\n\
                         cache-control: no-cache\r\nconnection: close\r\n\r\n\
                         data: {{\"choices\":[{{\"delta\":{{\"content\":{}}}}}]}}\n\n\
                         data: {{\"choices\":[{{\"delta\":{{}},\"finish_reason\":\"stop\"}}]}}\n\n\
                         data: [DONE]\n\n",
                        serde_json::to_string(&format!(
                            r#"{{"verdict":"continue","reason":"review {n}"}}"#,
                            n = reviews.load(std::sync::atomic::Ordering::SeqCst)
                        ))
                        .unwrap()
                    );
                    let _ = sock.write_all(sse.as_bytes());
                    return;
                }
                // Hold the KICKOFF turn open until the test has aborted.
                seen.store(true, std::sync::atomic::Ordering::SeqCst);
                let (lock, cv) = &*g;
                let mut open = lock.lock().unwrap();
                let deadline = std::time::Instant::now() + std::time::Duration::from_secs(8);
                while !*open && std::time::Instant::now() < deadline {
                    let (guard, _) = cv
                        .wait_timeout(open, std::time::Duration::from_millis(50))
                        .unwrap();
                    open = guard;
                }
                drop(open);
                let sse = "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\n\
                           cache-control: no-cache\r\nconnection: close\r\n\r\n\
                           data: {\"choices\":[{\"delta\":{\"content\":\"step done\"}}]}\n\n\
                           data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n\
                           data: {\"choices\":[],\"usage\":{\"prompt_tokens\":10,\
                           \"completion_tokens\":2,\"total_tokens\":12}}\n\n\
                           data: [DONE]\n\n";
                let _ = sock.write_all(sse.as_bytes());
            });
        }
    });
    let st = setup(addr, "ses_goalabort");
    ocserve_store::session_grant_always(&st.writer, &st.db, "ses_goalabort", "bash", "*").unwrap();
    let a = router(&st);
    // The v1 command runs the kickoff IN the request, so it blocks: arm in a
    // task and abort once the provider reports the turn is in flight.
    let a2 = a.clone();
    let arm = tokio::spawn(async move {
        req(
            &a2,
            "POST",
            "/session/ses_goalabort/command",
            Some(json!({"command": "goal", "arguments": "loop forever"})),
        )
        .await
    });
    let mut waited = 0;
    while !seen.load(std::sync::atomic::Ordering::SeqCst) && waited < 2_000 {
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        waited += 10;
    }
    assert!(
        seen.load(std::sync::atomic::Ordering::SeqCst),
        "the kickoff turn never reached the provider"
    );
    let _ = req(&a, "POST", "/session/ses_goalabort/abort", None).await;
    // Release the provider so the interrupted turn can unwind.
    {
        let (lock, cv) = &*gate;
        *lock.lock().unwrap() = true;
        cv.notify_all();
    }
    let g = wait_goal(&st, "ses_goalabort", "paused", 10).await;
    assert!(
        g["reason"]
            .as_str()
            .unwrap_or("")
            .to_lowercase()
            .contains("user"),
        "abort must pause with the user reason: {g}"
    );
    // No further round: the loop must not restart after the abort.
    let rounds_at_pause = g["rounds"].as_i64().unwrap_or(0);
    let reviews_at_pause = reviews.load(std::sync::atomic::Ordering::SeqCst);
    tokio::time::sleep(std::time::Duration::from_millis(800)).await;
    let after = ocserve_store::get_goal(&st.db, "ses_goalabort")
        .unwrap()
        .expect("row");
    assert_eq!(
        after.status, "paused",
        "the loop must not restart after an abort"
    );
    assert_eq!(
        after.rounds, rounds_at_pause,
        "no continuation may run after the abort"
    );
    assert_eq!(
        reviews.load(std::sync::atomic::Ordering::SeqCst),
        reviews_at_pause,
        "no further reviewer call may run after the abort"
    );
    let _ = arm.await;
}

/// A goal reviewer is a separate background task from the turn driver. DELETE
/// must cancel it too, or a reviewer that finishes after deletion can attempt
/// to persist a continuation and register another driver.
#[tokio::test(flavor = "multi_thread")]
async fn delete_cancels_a_pending_goal_review() {
    let (addr, entered, closed) = spawn_provider_holding_reviewer();
    let st = setup(addr, "ses_goaldelete_review");
    let app = router(&st);
    let kickoff = req(
        &app,
        "POST",
        "/session/ses_goaldelete_review/command",
        Some(json!({"command": "goal", "arguments": "finish the task"})),
    )
    .await;
    assert_eq!(kickoff.status(), StatusCode::OK);
    tokio::task::spawn_blocking(move || {
        entered
            .recv_timeout(std::time::Duration::from_secs(5))
            .expect("goal reviewer must reach the held provider")
    })
    .await
    .unwrap();

    let deleted = req(&app, "DELETE", "/session/ses_goaldelete_review", None).await;
    assert_eq!(deleted.status(), StatusCode::OK);
    assert_eq!(json(deleted).await, Value::Bool(true));
    tokio::task::spawn_blocking(move || {
        closed
            .recv_timeout(std::time::Duration::from_secs(5))
            .expect("deleting a session must close its pending reviewer request")
    })
    .await
    .unwrap();
    assert!(st.goal_wakes.lock().is_empty());
    assert!(!st.goal_tasks.lock().contains_key("ses_goaldelete_review"));
    assert!(
        ocserve_store::get_goal(&st.db, "ses_goaldelete_review")
            .unwrap()
            .is_none()
    );
    // CONTROL: leave pending goal reviews out of cancel_driver; the provider
    // connection remains open and the close assertion goes red.
}

/// The kill switch must disable the feature on the REAL HTTP path, not only in
/// the runtime's discovery helper. The previous "switch off" test asserted an
/// unrelated property (unknown command → 400), so this branch was untested
/// (2026-11-05 review L4).
#[tokio::test(flavor = "multi_thread")]
async fn kill_switch_off_refuses_goal_and_writes_no_state() {
    let addr = spawn_provider(r#"{"verdict":"done","reason":"never called"}"#);
    // Production shape with the switch off: `build_commands` does not add the
    // built-in entry at all, so the template lookup finds nothing.
    let mut st = setup_with_goal_cmd(addr, "ses_goaloff", false);
    Arc::get_mut(&mut st).expect("unique").goals_enabled = false;
    let a = router(&st);
    let r = req(
        &a,
        "POST",
        "/session/ses_goaloff/command",
        Some(json!({"command": "goal", "arguments": "do the thing"})),
    )
    .await;
    // Freeze's shape for a command that is not registered.
    assert_eq!(r.status(), StatusCode::BAD_REQUEST);
    assert!(
        json(r).await["data"]["message"]
            .as_str()
            .unwrap()
            .contains("Command not found"),
        "with the switch off goal must not be dispatchable"
    );
    assert!(
        ocserve_store::get_goal(&st.db, "ses_goaloff")
            .unwrap()
            .is_none(),
        "a disabled goal command must write no state"
    );
}
