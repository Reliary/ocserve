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
use refine_http::{AppState, LlmRegistry, Payloads, Wires};
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

/// Fake provider: captures (request-head, body) for the FIRST request, then
/// answers with a completing SSE stream and holds the socket open.
fn spawn_provider() -> (
    std::net::SocketAddr,
    std::sync::mpsc::Receiver<(String, String)>,
) {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        for sock in listener.incoming().flatten() {
            let mut sock = sock;
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
            let body = String::from_utf8_lossy(&got[head_end..(head_end + clen).min(got.len())])
                .to_string();
            let _ = tx.send((head, body));
            let _ = std::io::Write::write_all(&mut sock, SSE_OK.as_bytes());
            let _ = std::io::Write::flush(&mut sock);
            std::thread::sleep(std::time::Duration::from_secs(3));
        }
    });
    (addr, rx)
}

fn find_sub(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

/// Session + AppState wiring exactly as stall.rs, provider = capturing fake.
fn setup(addr: std::net::SocketAddr, tag: &str) -> Arc<AppState> {
    let dir = std::env::temp_dir().join(format!("refine-hooks-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let db = refine_store::writer::db_path(&dir);
    let writer = std::sync::Arc::new(refine_store::Writer::spawn(db.clone()).unwrap());
    let blobs = std::sync::Arc::new(refine_store::BlobStore::new(dir.join("blobs")).unwrap());
    let llm = LlmRegistry {
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
    refine_store::insert_session(
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
        std::env::temp_dir().join(format!("refine-hooks-plug-{}-{}", std::process::id(), tag));
    std::fs::create_dir_all(&dir).unwrap();
    let entry = dir.join("plug.mjs");
    std::fs::write(&entry, plugin_js).unwrap();
    let host = refine_plugin::materialize_host(&dir.join("host")).unwrap();
    let sc = refine_plugin::Sidecar::spawn(&host, "http://127.0.0.1:9", "/work")
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
                        "messageId": format!("msg_{tag}000000000000000000001"),
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
    let app = refine_http::router(st.clone());
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

    // persisted row carries the mutation too (hook fires BEFORE insert)
    let conn = refine_store::pragma::open_reader(&st.db).unwrap();
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
}

#[tokio::test]
async fn negative_control_no_plugins_wire_is_clean() {
    // identical harness, NO sidecar: if this looks like the positive test
    // the positive test is theatre.
    let (addr, rx) = spawn_provider();
    let st = setup(addr, "neg");
    let app = refine_http::router(st.clone());
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
        body.contains("hello-neg"),
        "original text must be on the wire"
    );
}

const THROWING: &str = r#"
export const PluginModule = {
  id: "hooks-throwing",
  server: async () => ({
    "chat.message": async () => { throw new Error("boom"); },
    "chat.params": async () => { throw new Error("boom"); },
    "chat.headers": async () => { throw new Error("boom"); },
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
    let app = refine_http::router(st.clone());
    run_prompt(app.clone(), &st, "fail", "hello-fail").await;

    let (_head, body) = rx
        .recv_timeout(std::time::Duration::from_secs(5))
        .expect("captured request");
    assert!(
        body.contains("hello-fail"),
        "prompt must complete with original text when hooks throw"
    );
    assert!(!body.contains("[CM]"), "throwing hook must not mutate");
}
