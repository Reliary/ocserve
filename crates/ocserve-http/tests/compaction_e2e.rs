//! M6 §9 e2e: forced overflow (tiny model limit) → auto compaction rounds,
//! D1 cap = three processed compactions, then the fourth trigger finalizes
//! with a session.error notice. Structural asserts on the projection +
//! message graph (COMPACTION.md §9; unit suites own byte formats).
use axum::body::Body;
use axum::http::{Request, StatusCode};
use ocserve_http::{AppState, LlmRegistry, Payloads, Wires};
use serde_json::json;
use std::io::Read as _;
use std::sync::Arc;
use tower::ServiceExt;

/// Every response overflows usable (context=1000): prompt900 + completion200
/// = total1100 ≥ usable1000 → every finished turn re-triggers compaction.
const SSE_OVERFLOW: &str = "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\n\
                      cache-control: no-cache\r\nconnection: close\r\n\r\n\
                      data: {\"choices\":[{\"delta\":{\"content\":\"R\"}}]}\n\n\
                      data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n\
                      data: {\"choices\":[],\"usage\":{\"prompt_tokens\":900,\
                      \"completion_tokens\":200,\"total_tokens\":1100,\
                      \"prompt_tokens_details\":{\"cached_tokens\":500}}}\n\n\
                      data: [DONE]\n\n";

/// Minimal multi-connection fake provider (every request gets SSE_OVERFLOW).
fn spawn_overflow_provider() -> std::net::SocketAddr {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    std::thread::spawn(move || {
        for sock in listener.incoming().flatten() {
            std::thread::spawn(move || {
                let mut sock = sock;
                let _ = sock.set_read_timeout(Some(std::time::Duration::from_secs(5)));
                let mut got = Vec::new();
                let mut buf = [0u8; 8192];
                // read request head
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
                let _ = std::io::Write::write_all(&mut sock, SSE_OVERFLOW.as_bytes());
                let _ = std::io::Write::flush(&mut sock);
            });
        }
    });
    addr
}

fn setup(addr: std::net::SocketAddr) -> Arc<AppState> {
    let dir = std::env::temp_dir().join(format!("ocserve-m6ov-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let db = ocserve_store::writer::db_path(&dir);
    let writer = Arc::new(ocserve_store::Writer::spawn(db.clone()).unwrap());
    let blobs = Arc::new(ocserve_store::BlobStore::new(dir.join("blobs")).unwrap());
    let mut limits = std::collections::HashMap::new();
    limits.insert(
        ("fake".to_string(), "m".to_string()),
        json!({"context": 1000}),
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
            "id": "ses_ov", "projectID": "global", "directory": "/w",
            "path": "ses_ov", "slug": "ses_ov", "title": "ov", "version": "1",
            "time": {"created": 1, "updated": 2},
        }),
    )
    .unwrap();
    st
}

async fn run_prompt(app: axum::Router, st: &Arc<AppState>) {
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/session/ses_ov/prompt_async")
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({
                        "messageID": "msg_ov0000000000000000000001",
                        "parts": [{"type": "text", "text": "trigger overflow"}],
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
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
    panic!("prompt did not complete (compaction e2e)");
}

#[tokio::test]
async fn forced_overflow_runs_three_compactions_then_caps() {
    let addr = spawn_overflow_provider();
    let st = setup(addr);
    let app = ocserve_http::router(st.clone());
    run_prompt(app, &st).await;

    let conn = ocserve_store::pragma::open_reader(&st.db).unwrap();

    // D1 cap: exactly THREE processed compactions (pending increments only);
    // the fourth trigger emits session.error and finalizes the answer
    let rows = ocserve_store::compaction_rows(&conn, "ses_ov").unwrap();
    assert_eq!(rows.len(), 3, "D1 cap = three processed compactions");
    for r in &rows {
        assert!(r.auto, "auto-flow anchors carry auto=true");
        assert!(!r.overflow, "usage path, not media-strip overflow");
        assert!(
            r.summary_msg_id.is_some(),
            "every anchor processed (summary linked)"
        );
    }

    // message graph: seed + 4 generations + 3×(anchor, summary, autocontinue)
    let msgs = ocserve_store::load_messages(&st.db, "ses_ov", None).unwrap();
    assert_eq!(msgs.len(), 14, "seed + 4 gens + 3 compaction triples");

    let mut summaries = 0;
    let mut autocontinues = 0;
    let mut summary_texts = 0;
    for (info, parts) in &msgs {
        if info["role"] == "assistant" && info["summary"] == val_true() {
            summaries += 1;
            for p in parts {
                if p["type"] == "text" && p["text"].as_str().map(|t| !t.is_empty()).unwrap_or(false)
                {
                    summary_texts += 1;
                }
            }
        }
        for p in parts {
            if p["metadata"]["compaction_continue"] == val_true() {
                autocontinues += 1;
            }
        }
    }
    assert_eq!(summaries, 3, "engine produced three summaries");
    assert_eq!(summary_texts, 3, "summary text persisted from the stream");
    assert_eq!(
        autocontinues, 3,
        "autocontinue message per compaction (auto)"
    );

    // final message = the capped generation's answer (kept per D1)
    let (last_info, last_parts) = msgs.last().unwrap();
    assert_eq!(last_info["role"], "assistant");
    assert!(last_info["summary"].is_null() || last_info["summary"] != val_true());
    let text: String = last_parts
        .iter()
        .filter(|p| p["type"] == "text")
        .filter_map(|p| p["text"].as_str())
        .collect();
    assert_eq!(text, "R", "answer persisted");
    assert!(
        last_info["finish"].is_string(),
        "finished normally (cap path)"
    );

    // observability (audit A->C): compaction gauge counts every completed
    // compaction (repeated-compaction measurement gap, arXiv 2607.08032)
    // and provider cache-read tokens reach /metrics (K-METRICS extension).
    let app = ocserve_http::router(st.clone());
    let resp = app
        .oneshot(
            Request::builder()
                .method("GET")
                .uri("/metrics")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let bytes = axum::body::to_bytes(resp.into_body(), 64 * 1024 * 1024)
        .await
        .unwrap();
    let exposition = String::from_utf8_lossy(&bytes).to_string();
    let comp_line = exposition
        .lines()
        .find(|l| l.starts_with("ocserve_compaction_total{"))
        .unwrap_or_else(|| panic!("compaction gauge missing:\n{exposition}"));
    let comp_n: u64 = comp_line
        .rsplit(' ')
        .next()
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    assert!(comp_n >= 3, "expected >=3, got {comp_n}: {comp_line}");
    assert!(
        exposition.contains("ocserve_compaction_total{auto=\"true\"}"),
        "auto label missing:\n{exposition}"
    );
    let cache_line = exposition
        .lines()
        .find(|l| l.starts_with("ocserve_llm_cache_tokens_total{"))
        .unwrap_or_else(|| panic!("cache-read counter missing:\n{exposition}"));
    let cache_n: u64 = cache_line
        .rsplit(' ')
        .next()
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    assert!(cache_n > 0, "cache-read not accumulated: {cache_line}");
}

/// Local true literal for `==` Value comparisons.
fn val_true() -> serde_json::Value {
    serde_json::Value::Bool(true)
}
