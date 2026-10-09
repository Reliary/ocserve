//! K-AUTONOMY / K-TITLE / K-ALWAYS e2e: opt-in round cap with FULL
//! surfacing (the "just stopped, no error" class), freeze default titles +
//! first-message auto-title, persisted always-grants surviving a fresh
//! gate (restart semantics). Every failure mode has a live assertion; the
//! pure helpers are unit-tested in ocserve-core (autonomy_tests).
use axum::body::Body;
use axum::http::{Request, StatusCode};
use ocserve_http::{AppState, LlmRegistry, Payloads, Wires};
use serde_json::json;
use std::io::Read as _;
use std::sync::Arc;
use tower::ServiceExt;

fn final_sse() -> &'static str {
    "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\n\
     cache-control: no-cache\r\nconnection: close\r\n\r\n\
     data: {\"choices\":[{\"delta\":{\"content\":\"pong\"}}]}\n\n\
     data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n\
     data: {\"choices\":[],\"usage\":{\"prompt_tokens\":100,\
     \"completion_tokens\":5,\"total_tokens\":105}}\n\n\
     data: [DONE]\n\n"
}

fn tool_call_sse() -> &'static str {
    "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\n\
     cache-control: no-cache\r\nconnection: close\r\n\r\n\
     data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\
     \"id\":\"call_k1\",\"type\":\"function\",\"function\":{\
     \"name\":\"bash\",\"arguments\":\"{\\\"command\\\":\\\"echo K1\\\"}\"\
     }}]}}]}\n\n\
     data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"tool_calls\"}]}\n\n\
     data: [DONE]\n\n"
}

/// Always-final provider (auto-title / default-title tests).
fn spawn_final_provider() -> std::net::SocketAddr {
    spawn_provider(std::sync::Arc::new(std::sync::Mutex::new(true)))
}

/// Always-tool provider (cap test: every round returns a tool call, so the
/// round cap can trip at step 2).
fn spawn_tool_provider() -> std::net::SocketAddr {
    spawn_provider(std::sync::Arc::new(std::sync::Mutex::new(false)))
}

fn spawn_provider(final_mode: std::sync::Arc<std::sync::Mutex<bool>>) -> std::net::SocketAddr {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    std::thread::spawn(move || {
        for sock in listener.incoming().flatten() {
            let final_mode = final_mode.clone();
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
                let body = if *final_mode.lock().unwrap() {
                    final_sse()
                } else {
                    tool_call_sse()
                };
                let _ = std::io::Write::write_all(&mut sock, body.as_bytes());
                let _ = std::io::Write::flush(&mut sock);
            });
        }
    });
    addr
}

/// Wire state + a session with the given id/title. max_rounds stays at the
/// env-at-boot default (0 = unlimited) unless the test sets `st.max_rounds`
/// afterwards — pub field mutation, no process-env races.
fn setup(addr: std::net::SocketAddr, sid: &str, title: &str) -> Arc<AppState> {
    // whole sid in the path: parallel tests with similar 6-char suffixes
    // would otherwise share one dir and race remove/spawn
    let dir = std::env::temp_dir().join(format!("ocserve-aut-{}-{sid}", std::process::id()));
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
            "path": sid, "slug": sid, "title": title, "version": "1",
            "time": {"created": 1, "updated": 2},
        }),
    )
    .unwrap();
    st
}

async fn post_json(
    app: &axum::Router,
    uri: &str,
    body: serde_json::Value,
) -> axum::response::Response {
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

async fn get(app: &axum::Router, uri: &str) -> axum::response::Response {
    app.clone()
        .oneshot(
            Request::builder()
                .method("GET")
                .uri(uri)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap()
}

async fn body_json(resp: axum::response::Response) -> serde_json::Value {
    let bytes = axum::body::to_bytes(resp.into_body(), 4 * 1024 * 1024)
        .await
        .unwrap();
    serde_json::from_slice(&bytes).unwrap()
}

// ---------- store-level units (K-ALWAYS / K-TITLE / K-AUTONOMY) ----------

#[test]
fn persisted_always_grants_roundtrip_and_retag_gates_on_default_title() {
    let dir = tempfile::tempdir().unwrap();
    let db = ocserve_store::writer::db_path(dir.path());
    let writer = ocserve_store::Writer::spawn(db.clone()).unwrap();
    ocserve_store::insert_session(
        &writer,
        &json!({"id": "ses_a", "projectID": "global", "directory": "/w",
                 "path": "s", "slug": "s", "title": "", "version": "1",
                 "time": {"created": 1, "updated": 2}}),
    )
    .unwrap();
    ocserve_store::insert_session(
        &writer,
        &json!({"id": "ses_b", "projectID": "global", "directory": "/w",
                 "path": "s", "slug": "s", "title": "Mine", "version": "1",
                 "time": {"created": 1, "updated": 2}}),
    )
    .unwrap();

    // grant: idempotent append, JSON roundtrip
    ocserve_store::session_grant_always(&writer, &db, "ses_a", "bash", "echo hi *").unwrap();
    ocserve_store::session_grant_always(&writer, &db, "ses_a", "bash", "echo hi *").unwrap();
    ocserve_store::session_grant_always(&writer, &db, "ses_a", "edit", "*").unwrap();
    let keys = ocserve_store::session_always_keys(&db, "ses_a").unwrap();
    assert_eq!(keys.len(), 2, "idempotent: {keys:?}");
    assert!(keys.iter().any(|k| k.starts_with("bash\t")));
    assert!(
        ocserve_store::session_always_keys(&db, "ses_missing")
            .unwrap()
            .is_empty()
    );

    // retag: default (empty) retags; the freeze default also retags;
    // a user-named session NEVER does
    assert!(ocserve_store::retag_default_title(&writer, "ses_a", "named by prompt").unwrap());
    assert_eq!(
        ocserve_store::session_always_keys(&db, "ses_a")
            .unwrap()
            .len(),
        2,
        "retag must not touch permission grants"
    );
    assert!(!ocserve_store::retag_default_title(&writer, "ses_b", "stolen").unwrap());
    let t: String = {
        let conn = ocserve_store::pragma::open_reader(&db).unwrap();
        conn.query_row("SELECT title FROM session WHERE id='ses_b'", [], |r| {
            r.get(0)
        })
        .unwrap()
    };
    assert_eq!(t, "Mine", "named title preserved");
}

#[test]
fn mark_turn_stopped_finalizes_and_appends_a_searchable_part() {
    let dir = tempfile::tempdir().unwrap();
    let db = ocserve_store::writer::db_path(dir.path());
    let writer = ocserve_store::Writer::spawn(db.clone()).unwrap();
    ocserve_store::insert_session(
        &writer,
        &json!({"id": "ses_t", "projectID": "global", "directory": "/w",
                 "path": "s", "slug": "s", "title": "t", "version": "1",
                 "time": {"created": 1, "updated": 2}}),
    )
    .unwrap();
    // assistant WITHOUT time.completed (the ghost state)
    ocserve_store::insert_message(
        &writer,
        None,
        "ses_t",
        &json!({"id": "msg_ghost", "sessionID": "ses_t", "role": "assistant",
                 "time": {"created": 5}}),
        &[json!({"id": "prt_x", "type": "text", "text": "partial"})],
    )
    .unwrap();

    let part = ocserve_store::mark_turn_stopped(&writer, &db, "ses_t", "prt_stop", "over 1 rounds")
        .unwrap()
        .expect("assistant exists");
    assert_eq!(part["type"], "text");
    assert!(
        part["text"].as_str().unwrap().starts_with("[turn stopped]"),
        "{part}"
    );
    let (completed, parts): (Option<i64>, i64) = {
        let conn = ocserve_store::pragma::open_reader(&db).unwrap();
        let c = conn
            .query_row(
                "SELECT json_extract(info,'$.time.completed') FROM msg WHERE id='msg_ghost'",
                [],
                |r| r.get::<_, Option<i64>>(0),
            )
            .unwrap();
        let p = conn
            .query_row(
                "SELECT count(*) FROM msg_part WHERE message_id='msg_ghost' AND type='text'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        (c, p)
    };
    assert!(completed.is_some(), "ghost finalized");
    assert_eq!(parts, 2, "original + [turn stopped] part");
    // guard rule 4 behavioral: every new part is searchable
    let (hits, _more) = ocserve_store::search_parts(&db, "turn stopped", None, 10, 0).unwrap();
    assert!(
        hits.iter().any(|h| h.session_id == "ses_t"),
        "part_search projection missed the stopped part"
    );

    // no assistant at all → None (session.error is still the trace)
    ocserve_store::insert_session(
        &writer,
        &json!({"id": "ses_e", "projectID": "global", "directory": "/w",
                 "path": "s", "slug": "s", "title": "e", "version": "1",
                 "time": {"created": 1, "updated": 2}}),
    )
    .unwrap();
    assert!(
        ocserve_store::mark_turn_stopped(&writer, &db, "ses_e", "prt_z", "boom")
            .unwrap()
            .is_none()
    );
}

// ---------- freeze default title (K-TITLE) ----------

#[tokio::test]
async fn create_session_defaults_to_freeze_title_not_empty() {
    let addr = spawn_final_provider();
    let st = setup(addr, "ses_dflt_unused", "seed");
    let app = ocserve_http::router(st.clone());
    let resp = post_json(&app, "/session", json!({"directory": "/w"})).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let created = body_json(resp).await;
    let title = created["title"].as_str().unwrap();
    // freeze isDefaultTitle: `New session - YYYY-MM-DDTHH:MM:SS.mmmZ`
    let re = regex_lite_is_default(title);
    assert!(re, "default title shape violated: {title:?}");
}

/// Ported freeze regex (session.ts isDefaultTitle) without a regex dep.
fn regex_lite_is_default(title: &str) -> bool {
    let Some(rest) = title.strip_prefix("New session - ") else {
        return false;
    };
    // 2026-10-04T12:00:00.000Z
    let b = rest.as_bytes();
    b.len() == 24
        && b[0..4].iter().all(u8::is_ascii_digit)
        && b[4] == b'-'
        && b[5..7].iter().all(u8::is_ascii_digit)
        && b[7] == b'-'
        && b[8..10].iter().all(u8::is_ascii_digit)
        && b[10] == b'T'
        && b[11..13].iter().all(u8::is_ascii_digit)
        && b[13] == b':'
        && b[14..16].iter().all(u8::is_ascii_digit)
        && b[16] == b':'
        && b[17..19].iter().all(u8::is_ascii_digit)
        && b[19] == b'.'
        && b[20..23].iter().all(u8::is_ascii_digit)
        && b[23] == b'Z'
}

// ---------- auto-title after the first turn + negative control ----------

#[tokio::test]
async fn first_turn_titles_a_default_session_and_never_renames_a_named_one() {
    let addr = spawn_final_provider();
    let st = setup(addr, "ses_title000001", ""); // empty → freeze default at create… seeded empty
    let app = ocserve_http::router(st.clone());
    let mut rx = st.bus.subscribe();

    // negative control: named session survives an identical run
    ocserve_store::insert_session(
        &st.writer,
        &json!({"id": "ses_named000001", "projectID": "global", "directory": "/w",
                 "path": "s", "slug": "s", "title": "Keeper", "version": "1",
                 "time": {"created": 1, "updated": 2}}),
    )
    .unwrap();

    let prompt = json!({
        "messageId": "msg_kt0000000000000000000001",
        "parts": [{"type": "text", "text": "  fix the\n  overnight   watcher please"}],
        "model": {"providerID": "fake", "modelID": "m"},
        "agent": "build",
    });
    let resp = post_json(&app, "/session/ses_title000001/message", prompt.clone()).await;
    assert_eq!(resp.status(), StatusCode::OK, "final_sse run must succeed");

    let got = body_json(get(&app, "/session/ses_title000001").await).await;
    let title = got["title"].as_str().unwrap();
    assert_eq!(title, "fix the overnight watcher please", "excerpt title");

    // session.updated with the new title must have been published
    let mut saw = false;
    for _ in 0..200 {
        match tokio::time::timeout(std::time::Duration::from_millis(20), rx.recv()).await {
            Ok(Ok(frame)) => {
                if frame.contains("session.updated")
                    && frame.contains("fix the overnight watcher please")
                {
                    saw = true;
                    break;
                }
            }
            _ => break,
        }
    }
    assert!(
        saw,
        "session.updated with the retagged title never published"
    );

    // negative control: named title untouched by an identical run
    let resp = post_json(
        &app,
        "/session/ses_named000001/message",
        json!({
            "messageId": "msg_kt0000000000000000000002",
            "parts": [{"type": "text", "text": "steal this name"}],
            "model": {"providerID": "fake", "modelID": "m"},
            "agent": "build",
        }),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::OK);
    let named = body_json(get(&app, "/session/ses_named000001").await).await;
    assert_eq!(
        named["title"], "Keeper",
        "named session must never be retagged"
    );
}

// ---------- the incident: cap → surfaced, never silent (K-AUTONOMY) ----------

#[tokio::test]
async fn round_cap_fails_loud_session_error_stopped_part_metric_and_persisted_grant() {
    let addr = spawn_tool_provider();
    let mut st = setup(addr, "ses_cap000000000001", "cap");
    // pub-field knob: ambient cap for THIS state only (no env races —
    // unique Arc, set before the router clones it)
    Arc::get_mut(&mut st).unwrap().max_rounds = 1;
    let app = ocserve_http::router(st.clone());

    let resp = post_json(
        &app,
        "/session/ses_cap000000000001/prompt_async",
        json!({
            "messageId": "msg_kc0000000000000000000001",
            "parts": [{"type": "text", "text": "keep going"}],
            "model": {"providerID": "fake", "modelID": "m"},
            "agent": "build",
        }),
    )
    .await;
    assert_eq!(resp.status(), StatusCode::NO_CONTENT);

    // answer the default rule ask (always → also exercises grant persist),
    // then wait for the cap death (step 2 > 1) and its session.error
    let mut saw_cap_error = false;
    for _ in 0..800 {
        {
            let conn = ocserve_store::pragma::open_reader(&st.db).unwrap();
            // reply to unanswered asks
            let asked: Vec<(String, String, bool)> = conn
                .prepare(
                    "SELECT json_extract(payload,'$.id'), json_extract(payload,'$.permission'), \
                     (SELECT count(*) FROM event e2 WHERE e2.session_id=event.session_id \
                      AND e2.type='permission.replied' \
                      AND json_extract(e2.payload,'$.requestID') = json_extract(event.payload,'$.id')) \
                     FROM event WHERE session_id='ses_cap000000000001' AND type='permission.asked'",
                )
                .unwrap()
                .query_map([], |r| {
                    Ok((r.get(0)?, r.get(1)?, r.get::<_, i64>(2)? > 0))
                })
                .unwrap()
                .filter_map(|r| r.ok())
                .collect();
            for (id, _perm, replied) in asked {
                if replied {
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
            if conn
                .query_row(
                    "SELECT count(*) FROM event WHERE session_id='ses_cap000000000001' \
                     AND type='session.error' AND payload LIKE '%OCSERVE_PROMPT_MAX_ROUNDS%'",
                    [],
                    |r| r.get::<_, i64>(0),
                )
                .unwrap_or(0)
                > 0
            {
                saw_cap_error = true;
            }
        }
        if saw_cap_error && st.prompt_tasks.lock().is_empty() && st.prompt_locks.lock().is_empty() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
    assert!(
        st.prompt_tasks.lock().is_empty() && st.prompt_locks.lock().is_empty(),
        "run must release its locks after the cap"
    );
    assert!(
        saw_cap_error,
        "the cap death must emit a durable session.error naming the knob (the silent-stop bug)"
    );

    let conn = ocserve_store::pragma::open_reader(&st.db).unwrap();
    // durable [turn stopped] explanation on the last assistant message
    let stopped: i64 = conn
        .query_row(
            "SELECT count(*) FROM msg_part WHERE session_id='ses_cap000000000001' \
             AND type='text' AND inline LIKE '%[turn stopped]%' \
             AND inline LIKE '%OCSERVE_PROMPT_MAX_ROUNDS%'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(stopped, 1, "exactly one [turn stopped] part");
    // no ghost: every assistant message is completed
    let ghosts: i64 = conn
        .query_row(
            "SELECT count(*) FROM msg WHERE session_id='ses_cap000000000001' \
             AND role='assistant' AND json_extract(info,'$.time.completed') IS NULL",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(ghosts, 0, "cap must never leave an unfinished assistant");
    // K-MODEL-STATE: resolved model was persisted at PROMPT START — the cap
    // death never reaches finalize, yet the column must hold the run's model
    // (the exact "model reverts to deepseek" regression).
    let pm: Option<String> = conn
        .query_row(
            "SELECT model FROM session WHERE id='ses_cap000000000001'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    let pm = pm.expect("start-persist wrote model despite the cap death");
    assert!(pm.contains("fake"), "run used the fake provider: {pm}");

    // K-ALWAYS: the 'always' reply persisted past-process (restart proof:
    // a FRESH gate hydrating from the row sees the grant)
    let keys = ocserve_store::session_always_keys(&st.db, "ses_cap000000000001").unwrap();
    assert!(
        !keys.is_empty(),
        "always reply must persist to session.permission"
    );
    let fresh = ocserve_core::permission::PermissionGate::new();
    fresh.hydrate(&st.db, "ses_cap000000000001").unwrap();
    // keys are "<permission>\t<pattern>"; the hydrated gate covers the pattern.
    let (perm, pat) = keys[0].split_once('\t').expect("tab-format key");
    assert!(
        fresh.check_always("ses_cap000000000001", perm, pat),
        "hydrated gate must consult the persisted grant"
    );

    // telemetry: capped rounds recorded
    let resp = get(&app, "/metrics").await;
    let bytes = axum::body::to_bytes(resp.into_body(), 1024 * 1024)
        .await
        .unwrap();
    let exposition = String::from_utf8_lossy(&bytes).to_string();
    assert!(
        exposition
            .lines()
            .any(|l| l.starts_with("ocserve_prompt_rounds_total{")
                && l.contains("finish=\"capped\"")),
        "ocserve_prompt_rounds_total{{finish=\"capped\"}} missing:\n{exposition}"
    );
    // K-EFFICIENCY: prompt-phase RSS + reader-open accounting landed
    assert!(
        exposition
            .lines()
            .any(|l| l.starts_with("ocserve_prompt_rss_start_bytes")),
        "ocserve_prompt_rss_start_bytes missing:\n{exposition}"
    );
    assert!(
        exposition
            .lines()
            .any(|l| l.starts_with("ocserve_db_opens_total")),
        "ocserve_db_opens_total missing:\n{exposition}"
    );
}

// ---------- B1: prompt-start model persistence + PATCH model/agent ----------

#[test]
fn persist_prompt_model_writes_once_then_churns_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let db = ocserve_store::writer::db_path(dir.path());
    let writer = ocserve_store::Writer::spawn(db.clone()).unwrap();
    ocserve_store::insert_session(
        &writer,
        &json!({"id": "ses_pm", "projectID": "global", "directory": "/w",
                 "path": "s", "slug": "s", "title": "t", "version": "1",
                 "time": {"created": 1, "updated": 2}}),
    )
    .unwrap();
    let mj = |p: &str, m: &str| json!({"id": m, "providerID": p, "variant": "default"}).to_string();

    assert_eq!(
        ocserve_store::persist_prompt_model(
            &writer,
            "ses_pm",
            "build",
            &mj("opencode-go", "mimo-v2.6-flash")
        )
        .unwrap(),
        1,
        "first persist writes"
    );
    assert_eq!(
        ocserve_store::persist_prompt_model(
            &writer,
            "ses_pm",
            "build",
            &mj("opencode-go", "mimo-v2.6-flash")
        )
        .unwrap(),
        0,
        "identical values are churn-free"
    );
    assert_eq!(
        ocserve_store::persist_prompt_model(
            &writer,
            "ses_pm",
            "plan",
            &mj("opencode", "big-pickle")
        )
        .unwrap(),
        1,
        "changed model writes"
    );
    let (model, agent): (String, String) = {
        let conn = ocserve_store::pragma::open_reader(&db).unwrap();
        conn.query_row(
            "SELECT model, agent FROM session WHERE id='ses_pm'",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap()
    };
    assert_eq!(agent, "plan");
    assert!(model.contains("big-pickle"), "{model}");
}

#[tokio::test]
async fn patch_session_model_and_agent_roundtrip_with_validation() {
    let addr = spawn_final_provider();
    let st = setup(addr, "ses_patch00000001", "patch me");
    let app = ocserve_http::router(st.clone());

    let patch = |body: serde_json::Value| {
        let app = app.clone();
        async move {
            app.oneshot(
                Request::builder()
                    .method("PATCH")
                    .uri("/session/ses_patch00000001")
                    .header("content-type", "application/json")
                    .body(Body::from(body.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap()
        }
    };
    // freeze-ish model patch → row + wire carry it
    let resp = patch(json!({"model": {"id": "mimo-v2.6-flash", "providerID": "opencode-go", "variant": "default"}})).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let info = body_json(resp).await;
    assert_eq!(info["model"]["id"], "mimo-v2.6-flash");
    assert_eq!(info["model"]["providerID"], "opencode-go");

    // agent patch
    let resp = patch(json!({"agent": "plan"})).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let info = body_json(resp).await;
    assert_eq!(info["agent"], "plan");

    // invalid model shape → 400 BadRequest envelope
    let resp = patch(json!({"model": {"nope": 1}})).await;
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);

    // null clears model (resolve falls back to default)
    let resp = patch(json!({"model": null})).await;
    assert_eq!(resp.status(), StatusCode::OK);
    let model: Option<String> = {
        let conn = ocserve_store::pragma::open_reader(&st.db).unwrap();
        conn.query_row(
            "SELECT model FROM session WHERE id='ses_patch00000001'",
            [],
            |r| r.get(0),
        )
        .unwrap()
    };
    assert!(model.is_none(), "null clears: {model:?}");

    // unknown session → 404 (PATCH path; POST /session/{id} is not a route)
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("PATCH")
                .uri("/session/ses_nope000000000000")
                .header("content-type", "application/json")
                .body(Body::from(json!({"title": "x"}).to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
}
