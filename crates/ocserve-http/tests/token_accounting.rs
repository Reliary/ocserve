//! K-TOKENS — context/token accounting parity (the 2026-10-11 "800% context"
//! class).
//!
//! Upstream rules (v1.18.31, verified in the frozen source + live freeze DBs):
//!   1. an assistant message is REPLACED with each step's usage
//!      (processor.ts:459 `ctx.assistantMessage.tokens = usage.tokens`), and its
//!      `cost` is that STEP'S cost (probed live: every assistant message carries
//!      a per-step cost, not the turn sum);
//!   2. the session row ACCUMULATES lifetime usage — every step-finish part
//!      applies `tokens_x = tokens_x + part.tokens.x` (core/session/projector.ts
//!      applyUsage:89-108,326);
//!   3. the web UI's context meter sums the LAST assistant message's components
//!      over `model.limit.context` (session-context-metrics.ts:28-59).
//!
//! ocserve persisted the whole-TURN sum on the final message and overwrote the
//! session row with it. For an 11-step turn that inflated the last message to
//! ~8.7M tokens against a 1M limit — the "800%" meter — and made the
//! auto-compaction overflow check fire ~Nx early.
//!
//! This test drives a REAL two-step turn (stub SSE provider, no LLM): step 1
//! calls a no-permission-ask tool (todowrite), step 2 finishes, each with its
//! own distinct usage. It then asserts the per-step message tokens, the
//! accumulated session row, and the reasoning split. The shared checker has a
//! planted pre-fix control (final message carries the turn sum) that MUST flag.
use axum::body::Body;
use axum::http::{Request, StatusCode};
use ocserve_http::{AppState, LlmRegistry, Payloads, Wires};
use serde_json::{Value, json};
use std::io::Read as _;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use tower::ServiceExt;

/// Step 1: a todowrite tool call (validated + persisted without a permission
/// ask) with usage {prompt 1000, completion 10, total 1010, cached 400}.
const STEP1_SSE: &str = "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\n\
     cache-control: no-cache\r\nconnection: close\r\n\r\n\
     data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\
     \"id\":\"call_tok1\",\"type\":\"function\",\"function\":{\
     \"name\":\"todowrite\",\"arguments\":\"{\\\"todos\\\":[{\\\"content\\\":\\\"probe\\\",\\\"status\\\":\\\"pending\\\"}]}\"\
     }}]}}]}\n\n\
     data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"tool_calls\"}]}\n\n\
     data: {\"choices\":[],\"usage\":{\"prompt_tokens\":1000,\
     \"completion_tokens\":10,\"total_tokens\":1010,\
     \"prompt_tokens_details\":{\"cached_tokens\":400}}}\n\n\
     data: [DONE]\n\n";

/// Step 2: final text with usage {prompt 2000, completion 20, total 2020,
/// cached 800, reasoning 5} — reasoning must split OUT of output.
const STEP2_SSE: &str = "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\n\
     cache-control: no-cache\r\nconnection: close\r\n\r\n\
     data: {\"choices\":[{\"delta\":{\"content\":\"done\"}}]}\n\n\
     data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n\
     data: {\"choices\":[],\"usage\":{\"prompt_tokens\":2000,\
     \"completion_tokens\":20,\"total_tokens\":2020,\
     \"prompt_tokens_details\":{\"cached_tokens\":800},\
     \"completion_tokens_details\":{\"reasoning_tokens\":5}}}\n\n\
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

fn setup(addr: std::net::SocketAddr, sid: &str) -> Arc<AppState> {
    let dir = std::env::temp_dir().join(format!("ocserve-tok-{}-{sid}", std::process::id()));
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

/// The shared per-step token contract checker. Returns human-readable
/// violations. `final_msg` is the LAST assistant message's `tokens`,
/// `session_input`/`session_output`/`session_cache_read` the session row's
/// accumulated columns, `expect_final` the step-2 usage projection.
fn token_violations(
    final_msg: &Value,
    session_input: i64,
    session_output: i64,
    session_cache_read: i64,
    expect_final: &Value,
    expect_session_input: i64,
) -> Vec<String> {
    let mut out = Vec::new();
    for key in ["input", "output", "reasoning"] {
        let got = final_msg[key].as_i64().unwrap_or(-1);
        let want = expect_final[key].as_i64().unwrap_or(-2);
        if got != want {
            out.push(format!(
                "final message tokens.{key} = {got}, want {want} (per-step REPLACE, \
                 not the whole-turn sum — the web UI meter reads this)"
            ));
        }
    }
    let gr = final_msg["cache"]["read"].as_i64().unwrap_or(-1);
    let wr = expect_final["cache"]["read"].as_i64().unwrap_or(-2);
    if gr != wr {
        out.push(format!(
            "final message cache.read = {gr}, want {wr} (per-step)"
        ));
    }
    if session_input != expect_session_input {
        out.push(format!(
            "session row tokens_input = {session_input}, want {expect_session_input} \
             (step-finish parts ACCUMULATE into the session row)"
        ));
    }
    // the per-step value must be strictly smaller than the lifetime aggregate —
    // if they are equal the final message is still carrying a multi-step total
    if final_msg["input"].as_i64().unwrap_or(0) >= session_input {
        out.push(format!(
            "final message input {} >= session lifetime input {session_input} — \
             the message is carrying a turn/session total, not one step",
            final_msg["input"].as_i64().unwrap_or(0)
        ));
    }
    let (so, sc) = (session_output, session_cache_read);
    // session output = step1 10 + step2 (20−5 reasoning) = 25; cache.read = 400+800
    if so != 25 || sc != 1200 {
        out.push(format!(
            "session row output={so} cache_read={sc}, want 25/1200"
        ));
    }
    out
}

/// Negative control (AGENTS §1): the checker MUST flag the exact pre-fix shape
/// (final message = whole-turn sum; session row overwritten with the same sum).
#[test]
fn token_checker_flags_the_pre_fix_turn_sum() {
    let step2 = json!({
        "total": 2020, "input": 1200, "output": 15, "reasoning": 5,
        "cache": {"write": 0, "read": 800}
    });
    // pre-fix: final message and session row both carried the turn sum
    // (input 600+1200, output 10+15+5, cache.read 400+800)
    let pre_fix = json!({
        "total": 3030, "input": 1800, "output": 30, "reasoning": 5,
        "cache": {"write": 0, "read": 1200}
    });
    let viol = token_violations(&pre_fix, 1800, 30, 1200, &step2, 1800);
    assert!(
        viol.iter().any(|v| v.contains("whole-turn sum") || v.contains("carrying a turn/session total")),
        "checker failed to flag the pre-fix shape: {viol:?}"
    );
    // healthy shape is clean (non-vacuous both directions)
    let ok = token_violations(&step2, 1800, 25, 1200, &step2, 1800);
    assert!(ok.is_empty(), "healthy values must be clean: {ok:?}");
}

#[tokio::test]
async fn per_step_message_tokens_and_accumulated_session_row() {
    let addr = spawn_seq_provider();
    let st = setup(addr, "ses_tok000000000000000001");
    let app = ocserve_http::router(st.clone());
    let sid = "ses_tok000000000000000001";

    let resp = post_json(
        &app,
        &format!("/session/{sid}/prompt_async"),
        json!({
            "messageID": "msg_kt0000000000000000000001",
            "parts": [{"type": "text", "text": "count"}],
            "model": {"providerID": "fake", "modelID": "m"},
            "agent": "build",
        }),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);

    // answer the todowrite ask(s) and wait for the turn to finish
    for _ in 0..800 {
        {
            let conn = ocserve_store::pragma::open_reader(&st.db).unwrap();
            let asked: Vec<(String, i64)> = conn
                .prepare(
                    "SELECT json_extract(payload,'$.id'), \
                     (SELECT count(*) FROM event e2 WHERE e2.session_id=event.session_id \
                      AND e2.type='permission.replied' \
                      AND json_extract(e2.payload,'$.requestID') = json_extract(event.payload,'$.id')) \
                     FROM event WHERE session_id=?1 AND type='permission.asked'",
                )
                .unwrap()
                .query_map([sid], |r| Ok((r.get(0)?, r.get(1)?)))
                .unwrap()
                .filter_map(|r| r.ok())
                .collect();
            for (id, replied) in asked {
                if replied > 0 {
                    continue;
                }
                let r = post_json(
                    &app,
                    &format!("/permission/{id}/reply"),
                    json!({"reply": "always"}),
                )
                .await;
                assert_eq!(r.status(), StatusCode::OK);
            }
        }
        if st.prompt_tasks.lock().is_empty() && st.prompt_locks.lock().is_empty() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }

    // the two assistant step messages, in order
    let conn = ocserve_store::pragma::open_reader(&st.db).unwrap();
    let mut stmt = conn
        .prepare(
            "SELECT info FROM msg WHERE session_id=?1 AND role='assistant' ORDER BY seq",
        )
        .unwrap();
    let msgs: Vec<Value> = stmt
        .query_map([sid], |r| r.get::<_, String>(0))
        .unwrap()
        .filter_map(|r| r.ok())
        .map(|s| serde_json::from_str::<Value>(&s).unwrap())
        .collect();
    assert_eq!(msgs.len(), 2, "expected two step messages: {msgs:?}");
    assert_eq!(msgs[0]["finish"], "tool-calls");
    assert_eq!(msgs[1]["finish"], "stop");

    let m1 = &msgs[0]["tokens"];
    let m2 = &msgs[1]["tokens"];
    // step 1: input = 1000−400, output 10, cache.read 400
    assert_eq!(m1["input"], 600, "step1 input {m1}");
    assert_eq!(m1["output"], 10, "step1 output {m1}");
    assert_eq!(m1["cache"]["read"], 400, "step1 cache.read {m1}");
    assert_eq!(m1["total"], 1010, "step1 total {m1}");
    // step 2: input = 2000−800, output = 20−5 (reasoning split), reasoning 5
    assert_eq!(m2["input"], 1200, "step2 input {m2}");
    assert_eq!(m2["output"], 15, "step2 output excludes reasoning {m2}");
    assert_eq!(m2["reasoning"], 5, "step2 reasoning {m2}");
    assert_eq!(m2["cache"]["read"], 800, "step2 cache.read {m2}");
    assert_eq!(m2["total"], 2020, "step2 total {m2}");

    // session row = step1 + step2
    let (si, so, sc, sr): (i64, i64, i64, i64) = conn
        .query_row(
            "SELECT tokens_input, tokens_output, tokens_cache_read, tokens_reasoning \
             FROM session WHERE id=?1",
            [sid],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )
        .unwrap();
    assert_eq!((si, so, sc, sr), (1800, 25, 1200, 5), "session accumulates both steps");

    // the shared contract checker (its negative control is proven above)
    let viol = token_violations(m2, si, so, sc, m2, 1800);
    assert!(
        viol.is_empty(),
        "token-accounting contract violations:\n  {}",
        viol.join("\n  ")
    );
}
