//! P0a plugin-dispatcher parity: chat.message / chat.params / chat.headers.
//!
//! Three-point battery against a capturing fake provider + the REAL node
//! sidecar with a synthetic fixture plugin:
//!   1. positive — mutations reach (a) the provider wire and (b) history;
//!   2. negative control — identical prompt with plugins disabled: the wire
//!      must differ (if it doesn't, the test is passing on its own
//!      scaffolding — TESTING §1 anti-theater);
//!   3. fail-open — a throwing chat.message hook never breaks the prompt
//!      (M4b rule; the persisted part stays unmutated).
use axum::body::Body;
use axum::http::{Request, StatusCode};
use ocserve_http::{AppState, LlmRegistry, Payloads, Wires};
use serde_json::json;
use std::io::Read as _;
use std::sync::Arc;
use tower::ServiceExt;

/// Minimal OpenAI-compatible SSE completion (content delta + finish + DONE).
const SSE_OK: &str = "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\n\
                      cache-control: no-cache\r\nconnection: close\r\n\r\n\
                      data: {\"choices\":[{\"delta\":{\"content\":\"reply-ok\"}}]}\n\n\
                      data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n\
                      data: [DONE]\n\n";

fn require_node() {
    let ok = std::process::Command::new("node")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);
    assert!(
        ok,
        "node required for hook-dispatch tests (M4b environment gate)"
    );
}

/// Fake provider: each request captures (head, body) then gets the next
/// scripted response (last repeats) — global-per-provider request counter so
/// the tool-loop test can script step1=tool_calls, step2=final.
fn spawn_provider_scripted(
    script: Vec<String>,
) -> (
    std::net::SocketAddr,
    std::sync::mpsc::Receiver<(String, String)>,
) {
    use std::sync::Arc as _Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let (tx, rx) = std::sync::mpsc::channel();
    let counter = _Arc::new(AtomicUsize::new(0));
    std::thread::spawn(move || {
        for sock in listener.incoming().flatten() {
            let mut sock = sock;
            let tx = tx.clone();
            let counter = counter.clone();
            let script = script.clone();
            let handle = std::thread::spawn(move || {
                let _ = sock.set_read_timeout(Some(std::time::Duration::from_secs(5)));
                let mut got = Vec::new();
                let mut buf = [0u8; 8192];
                // read head
                let head_end;
                loop {
                    match sock.read(&mut buf) {
                        Ok(0) => return,
                        Ok(n) => got.extend_from_slice(&buf[..n]),
                        Err(_) => return,
                    }
                    if let Some(pos) = find_sub(&got, b"\r\n\r\n") {
                        head_end = pos + 4;
                        break;
                    }
                }
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
                let body =
                    String::from_utf8_lossy(&got[head_end..(head_end + clen).min(got.len())])
                        .to_string();
                let idx = counter.fetch_add(1, Ordering::SeqCst);
                let resp = script[idx.min(script.len() - 1)].clone();
                let _ = tx.send((head, body));
                let _ = std::io::Write::write_all(&mut sock, resp.as_bytes());
                let _ = std::io::Write::flush(&mut sock);
                std::thread::sleep(std::time::Duration::from_millis(200));
            });
            let _ = handle.join();
        }
    });
    (addr, rx)
}

/// Single completing response (the default: one-step prompts).
fn spawn_provider() -> (
    std::net::SocketAddr,
    std::sync::mpsc::Receiver<(String, String)>,
) {
    spawn_provider_scripted(vec![SSE_OK.to_string()])
}

fn find_sub(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

/// Session + AppState wiring exactly as stall.rs, provider = capturing fake.
fn setup(addr: std::net::SocketAddr, tag: &str) -> Arc<AppState> {
    let dir = std::env::temp_dir().join(format!("ocserve-hooks-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
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
        &json!({
            "id": format!("ses_{tag}"), "projectID": "global", "directory": "/w",
            "path": format!("ses_{tag}"), "slug": format!("ses_{tag}"),
            "title": "hooks", "version": "1",
            "time": {"created": 1, "updated": 2},
        }),
    )
    .unwrap();
    st
}

/// Spawn the real sidecar and load `plugin_js` as a local entry file.
async fn load_plugin(st: &AppState, tag: &str, plugin_js: &str) {
    let dir =
        std::env::temp_dir().join(format!("ocserve-hooks-plug-{}-{}", std::process::id(), tag));
    std::fs::create_dir_all(&dir).unwrap();
    let entry = dir.join("plug.mjs");
    std::fs::write(&entry, plugin_js).unwrap();
    let host = ocserve_plugin::materialize_host(&dir.join("host")).unwrap();
    let sc = ocserve_plugin::Sidecar::spawn(&host, "http://127.0.0.1:9", "/work")
        .await
        .expect("spawn sidecar");
    let mut sc = sc;
    sc.load("hooks-fixture", &entry, &json!({}))
        .await
        .expect("load fixture plugin");
    if st
        .plugins
        .set(Arc::new(tokio::sync::Mutex::new(sc)))
        .is_err()
    {
        panic!("plugins already set");
    }
}

/// POST prompt_async and wait for the background prompt to finish.
async fn run_prompt(app: axum::Router, st: &Arc<AppState>, tag: &str, text: &str) {
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/session/ses_{tag}/prompt_async"))
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({
                        "messageID": format!("msg_{tag}000000000000000000001"),
                        "parts": [{"type": "text", "text": text}],
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
    for _ in 0..400 {
        if st.prompt_tasks.lock().is_empty() && st.prompt_locks.lock().is_empty() {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
    panic!("prompt did not complete (hooks test)");
}

/// Step1 response: model asks to run `echo hi` (original args — the hook
/// must rewrite them to `echo HOOKED_B4` BEFORE execution).
const SSE_TOOL: &str = "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\n\
                      cache-control: no-cache\r\nconnection: close\r\n\r\n\
                      data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\
                      \"id\":\"call_t1\",\"function\":{\"name\":\"bash\",\
                      \"arguments\":\"{\\\"command\\\":\\\"echo hi\\\"}\"}}]}}]}\n\n\
                      data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"tool_calls\"}]}\n\n\
                      data: [DONE]\n\n";

const FIXTURE: &str = r#"
export const PluginModule = {
  id: "hooks-fixture",
  server: async () => ({
    "chat.message": async (input, output) => {
      if (!input.sessionID || !input.messageID) throw new Error("chat.message input incomplete");
      output.message = { ...output.message, hooked: true };
      output.parts = output.parts.map((p) =>
        p.type === "text" ? { ...p, text: p.text + " [CM]" } : p);
    },
    "chat.params": async (_i, output) => {
      output.temperature = 0.7;
      output.options = { x_hook: true };
    },
    "chat.headers": async (_i, output) => {
      output.headers["x-plugin"] = "yes";
    },
    "experimental.chat.system.transform": async (_i, output) => {
      output.system.push("HOOKSYS");
    },
    "experimental.text.complete": async (_i, output) => {
      output.text = output.text + " [TC]";
    },
    "experimental.chat.messages.transform": async (_i, output) => {
      output.messages.push({ role: "user", content: "[MT]" });
    },
    "tool.execute.before": async (_i, output) => {
      if (output.args && typeof output.args.command === "string") {
        output.args.command = "echo HOOKED_B4";
      }
    },
    "command.execute.before": async (_i, output) => {
      output.parts = output.parts.map((p) =>
        p.type === "text" ? { ...p, text: p.text + " [CMD]" } : p);
    },
    "experimental.session.compacting": async (_i, output) => {
      output.prompt = "CUSTOMPROMPT-XYZ";
    },
  }),
};
export default PluginModule.server;
"#;

#[tokio::test]
async fn hook_mutations_reach_wire_and_history() {
    require_node();
    let (addr, rx) = spawn_provider();
    let st = setup(addr, "pos");
    load_plugin(&st, "pos", FIXTURE).await;
    let app = ocserve_http::router(st.clone());
    run_prompt(app.clone(), &st, "pos", "hello-pos").await;

    let (head, body) = rx
        .recv_timeout(std::time::Duration::from_secs(5))
        .expect("captured request");
    let hl = head.to_ascii_lowercase();
    assert!(
        hl.contains("x-plugin: yes"),
        "chat.headers missing:\n{head}"
    );
    assert!(
        body.contains("\"temperature\":0.7"),
        "chat.params temperature missing:\n{body}"
    );
    assert!(
        body.contains("\"x_hook\":true"),
        "chat.params options merge missing:\n{body}"
    );
    assert!(
        body.contains("[CM]"),
        "chat.message part mutation must reach the provider via history:\n{body}"
    );

    // P0b: system.transform mutation rides the request prefix
    assert!(
        body.contains("HOOKSYS"),
        "system.transform missing on wire:\n{body}"
    );

    // persisted rows carry the mutations (chat.message pre-insert; text.
    // complete pre-persist)
    let conn = ocserve_store::pragma::open_reader(&st.db).unwrap();
    let n: i64 = conn
        .query_row(
            "SELECT count(*) FROM msg_part WHERE session_id='ses_pos' AND inline LIKE '%[CM]%'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert!(
        n >= 1,
        "persisted user part must carry the [CM] mutation, got {n}"
    );
    let tc: i64 = conn
        .query_row(
            "SELECT count(*) FROM msg_part WHERE session_id='ses_pos' AND inline LIKE '%[TC]%'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert!(tc >= 1, "text.complete mutation must persist, got {tc}");

    // P0c: messages.transform appended message reaches the provider
    assert!(body.contains("[MT]"), "messages.transform missing:\n{body}");
}

#[tokio::test]
async fn negative_control_no_plugins_wire_is_clean() {
    // identical harness, NO sidecar: if this looks like the positive test
    // the positive test is theatre.
    let (addr, rx) = spawn_provider();
    let st = setup(addr, "neg");
    let app = ocserve_http::router(st.clone());
    run_prompt(app.clone(), &st, "neg", "hello-neg").await;

    let (head, body) = rx
        .recv_timeout(std::time::Duration::from_secs(5))
        .expect("captured request");
    let hl = head.to_ascii_lowercase();
    assert!(
        !hl.contains("x-plugin"),
        "unexpected plugin header:\n{head}"
    );
    assert!(
        !body.contains("temperature"),
        "unexpected temperature:\n{body}"
    );
    assert!(!body.contains("[CM]"), "unexpected mutation:\n{body}");
    assert!(
        !body.contains("HOOKSYS"),
        "unexpected system transform:\n{body}"
    );
    assert!(
        body.contains("hello-neg"),
        "original text must be on the wire"
    );
    let conn = ocserve_store::pragma::open_reader(&st.db).unwrap();
    let tc: i64 = conn
        .query_row(
            "SELECT count(*) FROM msg_part WHERE session_id='ses_neg' AND inline LIKE '%[TC]%'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(tc, 0, "no text.complete mutation without plugins");
    assert!(
        !body.contains("[MT]"),
        "unexpected messages.transform:\n{body}"
    );
}

const THROWING: &str = r#"
export const PluginModule = {
  id: "hooks-throwing",
  server: async () => ({
    "chat.message": async () => { throw new Error("boom"); },
    "chat.params": async () => { throw new Error("boom"); },
    "chat.headers": async () => { throw new Error("boom"); },
    "experimental.chat.system.transform": async () => { throw new Error("boom"); },
    "experimental.text.complete": async () => { throw new Error("boom"); },
    "experimental.chat.messages.transform": async () => { throw new Error("boom"); },
    "tool.execute.before": async () => { throw new Error("boom"); },
    "command.execute.before": async () => { throw new Error("boom"); },
  }),
};
export default PluginModule.server;
"#;

#[tokio::test]
async fn throwing_hooks_fail_open_prompt_completes() {
    require_node();
    let (addr, rx) = spawn_provider();
    let st = setup(addr, "fail");
    load_plugin(&st, "fail", THROWING).await;
    let app = ocserve_http::router(st.clone());
    run_prompt(app.clone(), &st, "fail", "hello-fail").await;

    let (_head, body) = rx
        .recv_timeout(std::time::Duration::from_secs(5))
        .expect("captured request");
    assert!(
        body.contains("hello-fail"),
        "prompt must complete with original text when hooks throw"
    );
    assert!(!body.contains("[CM]"), "throwing hook must not mutate");
    assert!(
        !body.contains("HOOKSYS"),
        "throwing system hook must not mutate"
    );
    let conn = ocserve_store::pragma::open_reader(&st.db).unwrap();
    let tc: i64 = conn
        .query_row(
            "SELECT count(*) FROM msg_part WHERE session_id='ses_fail' AND inline LIKE '%[TC]%'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(tc, 0, "throwing text.complete must not mutate");
    assert!(
        !body.contains("[MT]"),
        "throwing messages.transform must not mutate"
    );
}

#[tokio::test]
async fn tool_execute_before_mutates_execution() {
    require_node();
    // step1 asks for bash echo hi; step2 is the final text turn — the hook
    // rewrites args.command, so the EXECUTED output (visible only in the
    // step2 request as the tool result) must carry HOOKED_B4.
    let (addr, rx) = spawn_provider_scripted(vec![SSE_TOOL.to_string(), SSE_OK.to_string()]);
    let st = setup(addr, "tool");
    // default permission action is "ask" — inject allow so the gate does not
    // wait (the test has no client to answer permission prompts)
    st.payloads.write().agent = vec![json!({
        "name": "build",
        "permission": [{"permission": "bash", "pattern": "*", "action": "allow"}],
    })];
    load_plugin(&st, "tool", FIXTURE).await;
    let app = ocserve_http::router(st.clone());
    run_prompt(app.clone(), &st, "tool", "run it").await;

    let (_h1, b1) = rx
        .recv_timeout(std::time::Duration::from_secs(5))
        .expect("step1 capture");
    // step1 = conversation only (tool args live in the provider RESPONSE)
    assert!(
        !b1.contains("HOOKED_B4"),
        "step1 must precede execution:\n{b1}"
    );
    let (_h2, b2) = rx
        .recv_timeout(std::time::Duration::from_secs(5))
        .expect("step2 capture");
    assert!(
        b2.contains("HOOKED_B4"),
        "tool.execute.before mutation must reach EXECUTION (step2 tool result):\n{b2}"
    );
    // original args preserved in history/state (pre-hook) …
    assert!(
        b2.contains("echo hi"),
        "step2 history must keep the model's ORIGINAL args (state pre-hook):\n{b2}"
    );
    // … but the EXECUTED output (tool result) must be the mutated command's
    assert!(
        !b2.contains("\"output\":\"hi\""),
        "original command must not be what executed:\n{b2}"
    );
}

#[tokio::test]
async fn command_execute_before_mutates_parts() {
    require_node();
    let (addr, rx) = spawn_provider();
    let st = setup(addr, "cmd");
    st.payloads.write().command = vec![json!({"name": "hello", "template": "greet $1"})];
    load_plugin(&st, "cmd", FIXTURE).await;
    let app = ocserve_http::router(st.clone());
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/session/ses_cmd/command")
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({"command": "hello", "arguments": "world"}).to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK, "command must run");
    let (_h, _b) = rx
        .recv_timeout(std::time::Duration::from_secs(5))
        .expect("provider capture");
    // mutated parts flow into the prompt → persisted user part carries [CMD]
    let conn = ocserve_store::pragma::open_reader(&st.db).unwrap();
    let n: i64 = conn
        .query_row(
            "SELECT count(*) FROM msg_part WHERE session_id='ses_cmd' AND inline LIKE '%[CMD]%'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert!(
        n >= 1,
        "command.execute.before part mutation must persist, got {n}"
    );
}

#[tokio::test]
async fn plugin_event_pump_delivers_lifecycle_events() {
    require_node();
    let (addr, _rx) = spawn_provider();
    let st = setup(addr, "evt");
    // fixture: append each delivered event type to a log file — the file
    // only grows if pump → host → hooks["event"] actually ran
    let log = std::env::temp_dir().join(format!("ocserve-hooks-evtlog-{}.log", std::process::id()));
    let _ = std::fs::remove_file(&log);
    let fixture = format!(
        r#"
import {{ appendFileSync }} from "node:fs";
export const PluginModule = {{
  id: "evt-fixture",
  server: async () => ({{
    "event": async (payload) => {{
      appendFileSync({path:?}, payload.event.type + "\n");
    }},
  }}),
}};
export default PluginModule.server;
"#,
        path = log.to_string_lossy()
    );
    load_plugin(&st, "evt", &fixture).await;
    ocserve_http::start_plugin_event_pump(&st);
    let app = ocserve_http::router(st.clone());

    // session.created (route publish)
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/session")
                .header("content-type", "application/json")
                .body(Body::from(r#"{"title":"evt"}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let bytes = axum::body::to_bytes(resp.into_body(), 4096).await.unwrap();
    let created: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    let new_id = created["id"].as_str().unwrap().to_string();

    // message.removed (direct insert then DELETE)
    ocserve_store::insert_message(
        &st.writer,
        None,
        "ses_evt",
        &json!({
            "id": "msg_evt_del0000000000000000001", "sessionID": "ses_evt",
            "role": "assistant", "time": {"created": 1, "completed": 2},
        }),
        &[json!({
            "id": "prt_evt_del0000000000000000001", "sessionID": "ses_evt",
            "messageID": "msg_evt_del0000000000000000001",
            "type": "text", "text": "bye",
        })],
    )
    .unwrap();
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("DELETE")
                .uri("/session/ses_evt/message/msg_evt_del0000000000000000001")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    // session.deleted (route publish)
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("DELETE")
                .uri(format!("/session/{new_id}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    // poll: all three types must reach the plugin's event hook
    let mut got = String::new();
    for _ in 0..400 {
        got = std::fs::read_to_string(&log).unwrap_or_default();
        if got.contains("session.created")
            && got.contains("message.removed")
            && got.contains("session.deleted")
        {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
    assert!(
        got.contains("session.created")
            && got.contains("message.removed")
            && got.contains("session.deleted"),
        "event pump must deliver lifecycle events, got:\n{got}"
    );
}

#[tokio::test]
async fn summarize_fires_compacting_not_chat_message() {
    require_node();
    // two provider requests: (1) the seeding prompt (chat.message MUST fire)
    // (2) the summarize turn (compacting MUST fire, chat.message MUST NOT)
    let (addr, rx) = spawn_provider_scripted(vec![SSE_OK.to_string(), SSE_OK.to_string()]);
    let st = setup(addr, "sum");
    load_plugin(&st, "sum", FIXTURE).await;
    let app = ocserve_http::router(st.clone());
    run_prompt(app.clone(), &st, "sum", "seed-history").await;
    let (_h1, b1) = rx
        .recv_timeout(std::time::Duration::from_secs(5))
        .expect("prompt capture");
    assert!(b1.contains("[CM]"), "prompt flow must fire chat.message");

    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/session/ses_sum/summarize")
                .header("content-type", "application/json")
                .body(Body::from(r#"{"providerID":"fake","modelID":"m"}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK, "summarize must run");
    let (_h2, b2) = rx
        .recv_timeout(std::time::Duration::from_secs(5))
        .expect("summarize capture");
    // compacting hook REPLACED the prompt wholesale (compaction.ts:382)
    assert!(
        b2.contains("CUSTOMPROMPT-XYZ"),
        "experimental.session.compacting prompt override must reach the wire:\n{b2}"
    );
    // the summarize user marker must NOT carry chat.message's [CM] tag
    // (upstream summarize never triggers chat.message — fidelity gate on
    // skip_history, not persist_user)
    let conn = ocserve_store::pragma::open_reader(&st.db).unwrap();
    // exactly ONE [CM]-tagged part in the session — the seed prompt's.
    // (Summarize's user marker persists EMPTY parts, so counting rows for
    // it would pass vacuously; the invariant is: summarize adds no new
    // chat.message-mutated part.)
    let tagged: i64 = conn
        .query_row(
            "SELECT count(*) FROM msg_part WHERE session_id='ses_sum' AND inline LIKE '%[CM]%'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        tagged, 1,
        "summarize must not add a chat.message-mutated part (seed=1)"
    );
}
