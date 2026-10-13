//! K-SNAPSHOT — the snapshot engine wired into the agent loop (SNAPSHOT.md
//! P1). A real two-step turn (stub provider) whose step-1 tool (`write`) creates
//! a file: the persisted assistant message must carry a `patch` part recording
//! that file, and the `step-start` part must carry the pre-turn snapshot hash.
//!
//! The shared contract checker has a planted pre-fix control (no patch part)
//! that MUST flag, so a regression to the shape-only behavior goes red.
use axum::body::Body;
use axum::http::{Request, StatusCode};
use ocserve_http::{AppState, LlmRegistry, Payloads, Wires};
use serde_json::{Value, json};
use std::io::Read as _;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use tower::ServiceExt;

/// Step 1: a `write` tool call creating `made.txt` (permitted via an allow rule).
const STEP1_SSE: &str = "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\n\
     cache-control: no-cache\r\nconnection: close\r\n\r\n\
     data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\
     \"id\":\"call_snap1\",\"type\":\"function\",\"function\":{\
     \"name\":\"write\",\"arguments\":\"{\\\"filePath\\\":\\\"made.txt\\\",\\\"content\\\":\\\"hi\\\"}\"\
     }}]}}]}\n\n\
     data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"tool_calls\"}]}\n\n\
     data: {\"choices\":[],\"usage\":{\"prompt_tokens\":100,\
     \"completion_tokens\":5,\"total_tokens\":105}}\n\n\
     data: [DONE]\n\n";

const STEP2_SSE: &str = "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\n\
     cache-control: no-cache\r\nconnection: close\r\n\r\n\
     data: {\"choices\":[{\"delta\":{\"content\":\"done\"}}]}\n\n\
     data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n\
     data: {\"choices\":[],\"usage\":{\"prompt_tokens\":120,\
     \"completion_tokens\":6,\"total_tokens\":126}}\n\n\
     data: [DONE]\n\n";

fn spawn_seq_provider() -> std::net::SocketAddr {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    std::thread::spawn(move || {
        let n = AtomicUsize::new(0);
        for sock in listener.incoming().flatten() {
            let body = if n.fetch_add(1, Ordering::SeqCst) == 0 {
                STEP1_SSE
            } else {
                STEP2_SSE
            };
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
                let _ = std::io::Write::write_all(&mut sock, body.as_bytes());
                let _ = std::io::Write::flush(&mut sock);
            });
        }
    });
    addr
}

/// A git repo worktree used as the session directory. Returns (state, worktree).
fn setup(addr: std::net::SocketAddr, sid: &str) -> (Arc<AppState>, std::path::PathBuf) {
    let dir = std::env::temp_dir().join(format!("ocserve-snap-{}-{sid}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    // a real git repo worktree (session directory)
    let wt = dir.join("worktree");
    std::fs::create_dir_all(&wt).unwrap();
    let g = |args: &[&str]| {
        let o = std::process::Command::new("git")
            .current_dir(&wt)
            .args(args)
            .output()
            .unwrap();
        assert!(
            o.status.success(),
            "git {:?}: {}",
            args,
            String::from_utf8_lossy(&o.stderr)
        );
    };
    g(&["init", "-q"]);
    g(&["config", "user.email", "t@t"]);
    g(&["config", "user.name", "t"]);
    std::fs::write(wt.join("base.txt"), "base\n").unwrap();
    g(&["add", "."]);
    g(&["commit", "-qm", "init"]);

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
    // allow write + bash so no permission prompt blocks (no client to answer)
    let mut p = st.payloads.write();
    p.agent = vec![json!({
        "name": "build",
        "permission": [
            {"permission": "write", "pattern": "*", "action": "allow"},
            {"permission": "edit", "pattern": "*", "action": "allow"},
        ],
    })];
    drop(p);
    ocserve_store::insert_session(
        &st.writer,
        &json!({
            "id": sid, "projectID": "global", "directory": wt.display().to_string(),
            "path": sid, "slug": sid, "title": "t", "version": "1",
            "time": {"created": 1, "updated": 2},
        }),
    )
    .unwrap();
    (st, wt)
}

async fn post_json(app: &axum::Router, uri: &str, body: Value) -> axum::response::Response {
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
}

fn load_assistant_parts(st: &Arc<AppState>, sid: &str) -> Vec<Value> {
    let conn = rusqlite::Connection::open(&st.db).unwrap();
    let mut stmt = conn
        .prepare(
            "SELECT mp.inline FROM msg_part mp JOIN msg m ON m.id = mp.message_id \
             WHERE mp.session_id = ?1 AND m.role = 'assistant' AND mp.inline IS NOT NULL",
        )
        .unwrap();
    let parts: Vec<Value> = stmt
        .query_map([sid], |r| r.get::<_, String>(0))
        .unwrap()
        .filter_map(|r| r.ok())
        .filter_map(|s| serde_json::from_str::<Value>(&s).ok())
        .collect();
    parts
}

/// The shared snapshot-part contract checker. Returns violations. A planted
/// pre-fix control passes `patches = []` and MUST produce a violation.
fn snapshot_violations(parts: &[Value], patch_files: &[String]) -> Vec<String> {
    let mut out = Vec::new();
    let has_patch = parts.iter().any(|p| p["type"] == "patch");
    if !has_patch && !patch_files.is_empty() {
        out.push(format!(
            "no `patch` part persisted although the turn changed {} file(s) — the revert \
             file-restore path has nothing to restore (SNAPSHOT §2)",
            patch_files.len()
        ));
    }
    let step_start_has_snapshot = parts
        .iter()
        .filter(|p| p["type"] == "step-start")
        .any(|p| p["snapshot"].is_string());
    if !step_start_has_snapshot && !patch_files.is_empty() {
        out.push("no `step-start` part carries a snapshot hash (capture did not run)".into());
    }
    out
}

#[tokio::test]
async fn tool_turn_records_a_patch_part_and_start_snapshot() {
    let addr = spawn_seq_provider();
    let sid = "ses_snap00000000000000000001";
    let (st, wt) = setup(addr, sid);
    // Build the app so the router (and thus the prompt path) exists.
    let app = ocserve_http::router(st.clone());
    let resp = post_json(
        &app,
        &format!("/session/{sid}/prompt_async"),
        json!({"messageID": "msg_snap000000000000000000001", "parts": [{"type": "text", "text": "make a file"}], "model": {"providerID": "fake", "modelID": "m"}, "agent": "build"}),
    )
    .await;
    assert!(matches!(
        resp.status(),
        StatusCode::NO_CONTENT | StatusCode::OK
    ));

    // wait for the turn to finish (final assistant message with finish=stop)
    let mut done = false;
    for _ in 0..100 {
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        let conn = rusqlite::Connection::open(&st.db).unwrap();
        let n: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM msg WHERE info LIKE '%\"finish\":\"stop\"%'",
                [],
                |r| r.get(0),
            )
            .unwrap_or(0);
        if n > 0 {
            done = true;
            break;
        }
    }
    assert!(done, "turn did not finish");
    assert!(
        wt.join("made.txt").exists(),
        "the write tool must have created the file"
    );

    let parts = load_assistant_parts(&st, sid);
    let patch_files: Vec<String> = parts
        .iter()
        .filter(|p| p["type"] == "patch")
        .flat_map(|p| p["files"].as_array().cloned().unwrap_or_default())
        .filter_map(|f| f.as_str().map(String::from))
        .collect();

    // the real contract must hold
    let v = snapshot_violations(&parts, &["made.txt".to_string()]);
    assert!(v.is_empty(), "snapshot contract violations: {v:?}");
    assert!(
        patch_files.iter().any(|f| f.ends_with("made.txt")),
        "patch part must record made.txt: {patch_files:?}"
    );

    // PLANTED NEGATIVE CONTROL: with the patch parts removed (the pre-fix
    // shape), the SAME checker must flag — proving it is non-vacuous.
    let stripped: Vec<Value> = parts
        .iter()
        .filter(|p| p["type"] != "patch")
        .cloned()
        .collect();
    let control = snapshot_violations(&stripped, &["made.txt".to_string()]);
    assert!(
        !control.is_empty(),
        "checker is vacuous: it must flag a missing patch part"
    );
}
