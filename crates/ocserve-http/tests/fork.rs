//! K-FORK: POST /session/{id}/fork — freeze session.fork (session.ts:691).
//!
//! Covers: exclusive `slice(0, findIndex)` cut, unknown-id → copy-all quirk,
//! getForkedTitle chaining, id/parentID/tail_start_id remaps, verbatim
//! zero-copy blob sharing, search/compaction projections, source immutability,
//! rollback-visible envelopes (404/400), session.created emission (and the
//! documented D-FORK-1 absence of message/part SSE floods).

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use ocserve_http::{AppState, LlmRegistry, Payloads, Wires};
use serde_json::{Value, json};
use tower::ServiceExt;

fn state(dir: &std::path::Path, payloads: Payloads) -> std::sync::Arc<AppState> {
    let db = ocserve_store::writer::db_path(dir);
    let writer = std::sync::Arc::new(ocserve_store::Writer::spawn(db.clone()).unwrap());
    let blobs = std::sync::Arc::new(ocserve_store::BlobStore::new(dir.join("blobs")).unwrap());
    let llm = LlmRegistry {
        limits: std::collections::HashMap::new(),
        endpoints: [("fake".into(), ("http://127.0.0.1:9".into(), String::new()))]
            .into_iter()
            .collect(),
        pricing: Default::default(),
        default_model: ("fake".into(), "m".into()),
        systems: Default::default(),
        default_agent: "build".into(),
    };
    AppState::with_wiring(
        None,
        payloads,
        Wires {
            db,
            blobs,
            writer,
            llm,
        },
    )
}

async fn fork(app: &axum::Router, sid: &str, body: Option<&str>) -> (StatusCode, Vec<u8>) {
    let mut b = Request::builder()
        .method("POST")
        .uri(format!("/session/{sid}/fork"));
    if body.is_some() {
        b = b.header("content-type", "application/json");
    }
    let resp = app
        .clone()
        .oneshot(b.body(Body::from(body.unwrap_or("").to_string())).unwrap())
        .await
        .unwrap();
    let status = resp.status();
    let bytes = resp
        .into_body()
        .collect()
        .await
        .unwrap()
        .to_bytes()
        .to_vec();
    (status, bytes)
}

fn seed_session(st: &AppState, sid: &str, title: &str) {
    ocserve_store::insert_session(
        &st.writer,
        &json!({
            "id": sid, "projectID": "global", "directory": "/w", "path": sid,
            "slug": sid, "title": title, "version": "1",
            "time": {"created": 1, "updated": 2},
        }),
    )
    .unwrap();
}

/// 5 messages, chronological by (time_created, id), with the functional
/// fields fork must remap: parentID (valid + dangling), compaction
/// tail_start_id, and a >8 KiB blobbed part carrying a searchable token.
fn seed_messages(st: &AppState, sid: &str) {
    let blob_text = format!(
        "zebrafish-unique-token {}",
        "x".repeat(ocserve_store::INLINE_PART_MAX + 1024)
    );
    let msgs: Vec<(Value, Vec<Value>)> = vec![
        (
            json!({"id": "msg_t1", "role": "user", "time": {"created": 100}}),
            vec![json!({"id": "prt_1", "type": "text", "text": "hello"})],
        ),
        (
            json!({
                "id": "msg_t2", "role": "assistant", "parentID": "msg_zzz",
                "time": {"created": 200}
            }),
            vec![
                json!({"id": "prt_2", "type": "text", "text": "hi there"}),
                json!({"id": "prt_3", "type": "text", "text": blob_text}),
            ],
        ),
        (
            json!({"id": "msg_t3", "role": "user", "time": {"created": 300}}),
            vec![json!({"id": "prt_4", "type": "text", "text": "again"})],
        ),
        (
            json!({
                "id": "msg_t4", "role": "assistant", "parentID": "msg_t3",
                "summary": true, "time": {"created": 400}
            }),
            vec![json!({
                "id": "prt_5", "type": "compaction",
                "tail_start_id": "msg_t1", "auto": false, "overflow": false
            })],
        ),
        (
            json!({"id": "msg_t5", "role": "user", "time": {"created": 500}}),
            vec![json!({"id": "prt_6", "type": "text", "text": "final"})],
        ),
    ];
    for (info, parts) in msgs {
        ocserve_store::insert_message(&st.writer, Some(&*st.blobs), sid, &info, &parts).unwrap();
    }
}

async fn get_messages(app: &axum::Router, sid: &str) -> Vec<Value> {
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .uri(format!("/session/{sid}/message"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    serde_json::from_slice(&bytes).unwrap()
}

fn raw_rows(st: &AppState, sql: &str) -> Vec<Value> {
    let conn = ocserve_store::pragma::open_reader(&st.db).unwrap();
    let mut stmt = conn.prepare(sql).unwrap();
    let ncols = stmt.column_count();
    let names: Vec<String> = (0..ncols)
        .map(|i| stmt.column_name(i).unwrap().to_string())
        .collect();
    let mut rows = stmt.query([]).unwrap();
    let mut out = Vec::new();
    while let Some(row) = rows.next().unwrap() {
        let mut o = serde_json::Map::new();
        for (i, n) in names.iter().enumerate() {
            let v: serde_json::Value = match row.get::<_, Option<String>>(i) {
                Ok(Some(s)) => json!(s),
                Ok(None) => Value::Null,
                Err(_) => json!(row.get::<_, i64>(i).unwrap_or(-1)),
            };
            o.insert(n.clone(), v);
        }
        out.push(Value::Object(o));
    }
    out
}

#[tokio::test]
async fn fork_full_copy_remaps_ids_and_shares_blobs() {
    let dir = tempfile::tempdir().unwrap();
    let st = state(dir.path(), Payloads::default());
    let app = ocserve_http::router(st.clone());
    seed_session(&st, "ses_src", "My session");
    seed_messages(&st, "ses_src");
    let mut rx = st.bus.subscribe();

    let (status, bytes) = fork(&app, "ses_src", Some("{}")).await;
    assert_eq!(
        status,
        StatusCode::OK,
        "body: {}",
        String::from_utf8_lossy(&bytes)
    );
    let info: Value = serde_json::from_slice(&bytes).unwrap();
    let dst = info["id"].as_str().unwrap().to_string();
    assert!(dst.starts_with("ses_") && dst != "ses_src");
    assert_eq!(info["title"], "My session (fork #1)");
    assert_eq!(info["cost"], 0, "fork does not inherit usage");

    // 5 messages, NEW ids, chronological seq, time preserved
    let msgs = get_messages(&app, &dst).await;
    assert_eq!(msgs.len(), 5, "all messages copied (no cut)");
    let old_ids: Vec<String> = ["msg_t1", "msg_t2", "msg_t3", "msg_t4", "msg_t5"]
        .iter()
        .map(|s| s.to_string())
        .collect();
    let conn = ocserve_store::pragma::open_reader(&st.db).unwrap();
    let new_ids: Vec<String> = {
        let mut stmt = conn
            .prepare("SELECT id FROM msg WHERE session_id=?1 ORDER BY seq")
            .unwrap();
        let rows = stmt.query_map([&dst], |r| r.get::<_, String>(0)).unwrap();
        rows.collect::<std::result::Result<Vec<_>, _>>().unwrap()
    };
    assert_eq!(new_ids.len(), 5);
    for n in &new_ids {
        assert!(!old_ids.contains(n), "fresh message id: {n}");
    }
    // seq order = time order (100..500), times preserved
    let times: Vec<i64> = {
        let mut stmt = conn
            .prepare("SELECT time_created FROM msg WHERE session_id=?1 ORDER BY seq")
            .unwrap();
        let rows = stmt.query_map([&dst], |r| r.get::<_, i64>(0)).unwrap();
        rows.collect::<std::result::Result<Vec<_>, _>>().unwrap()
    };
    assert_eq!(times, vec![100, 200, 300, 400, 500]);

    // parentID: valid parent remapped to NEW id; dangling parent passes through
    let parsed: Vec<(String, Value)> = {
        let mut stmt = conn
            .prepare("SELECT id, info FROM msg WHERE session_id=?1 ORDER BY seq")
            .unwrap();
        let rows = stmt
            .query_map([&dst], |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
            })
            .unwrap()
            .collect::<std::result::Result<Vec<_>, _>>()
            .unwrap();
        rows.into_iter()
            .map(|(id, txt)| (id, serde_json::from_str::<Value>(&txt).unwrap()))
            .collect()
    };
    let parent_of = |i: usize| -> Option<String> {
        parsed[i]
            .1
            .get("parentID")
            .and_then(|p| p.as_str())
            .map(String::from)
    };
    let new_t3 = parsed[2].0.clone();
    assert_eq!(
        parent_of(3).as_deref(),
        Some(new_t3.as_str()),
        "valid parent remapped"
    );
    assert_eq!(
        parent_of(1).as_deref(),
        Some("msg_zzz"),
        "dangling parent untouched"
    );
    // every copied info carries the new sessionID
    for (id, v) in &parsed {
        assert_eq!(v["sessionID"], json!(dst), "info sessionID on {id}");
    }

    // blob part: SAME sha (zero-copy), dst inline NULL, content searchable
    let blob_rows = raw_rows(
        &st,
        &format!(
            "SELECT blob_sha, inline, byte_len FROM msg_part WHERE session_id='{dst}' \
             AND blob_sha IS NOT NULL"
        ),
    );
    assert_eq!(blob_rows.len(), 1, "one blobbed part in fork");
    let src_blob = raw_rows(&st, "SELECT blob_sha FROM msg_part WHERE id='prt_3'");
    assert_eq!(
        blob_rows[0]["blob_sha"], src_blob[0]["blob_sha"],
        "zero-copy: forked blob SHARES the source sha"
    );
    assert_eq!(blob_rows[0]["inline"], Value::Null);

    // compaction part: tail_start_id remapped to NEW msg_t1
    let comp = raw_rows(
        &st,
        &format!("SELECT inline FROM msg_part WHERE session_id='{dst}' AND type='compaction'"),
    );
    assert_eq!(comp.len(), 1);
    let cv: Value = serde_json::from_str(comp[0]["inline"].as_str().unwrap()).unwrap();
    assert_eq!(
        cv["tail_start_id"],
        json!(new_ids[0]),
        "tail_start_id remapped"
    );
    assert_ne!(cv["id"], json!("prt_5"), "fresh part id");
    // projections built for the fork (guard-rule-4 companions)
    let ps: i64 = {
        let mut stmt = conn
            .prepare("SELECT count(*) FROM part_search WHERE session_id=?1")
            .unwrap();
        stmt.query_row([&dst], |r| r.get(0)).unwrap()
    };
    let comp_n: i64 = {
        let mut stmt = conn
            .prepare("SELECT count(*) FROM compaction WHERE session_id=?1")
            .unwrap();
        stmt.query_row([&dst], |r| r.get(0)).unwrap()
    };
    assert!(ps >= 6, "part_search rows for every copied part: {ps}");
    assert_eq!(comp_n, 1, "compaction projection copied");

    // SOURCE UNTOUCHED (negative control: overwrite would corrupt it)
    let src_msgs = get_messages(&app, "ses_src").await;
    assert_eq!(src_msgs.len(), 5, "source message count");
    let src_parts: i64 = {
        let mut stmt = conn
            .prepare("SELECT count(*) FROM msg_part WHERE session_id='ses_src'")
            .unwrap();
        stmt.query_row([], |r| r.get(0)).unwrap()
    };
    assert_eq!(src_parts, 6, "source parts intact");
    let src_blob_rows = raw_rows(
        &st,
        "SELECT count(*) AS c FROM msg_part WHERE id='prt_3' AND blob_sha IS NOT NULL",
    );
    assert_eq!(src_blob_rows[0]["c"], json!(1), "source blob row intact");

    // D-FORK-1: session.created emitted; NO message/part SSE floods
    let mut created = 0;
    let mut msg_events = 0;
    while let Ok(frame) = rx.try_recv() {
        let s = frame.to_string();
        if s.contains("\"type\":\"session.created\"") && s.contains(&dst) {
            created += 1;
        }
        if s.contains("\"type\":\"message.updated\"") && s.contains(&dst) {
            msg_events += 1;
        }
        if s.contains("\"type\":\"message.part.updated\"") && s.contains(&dst) {
            msg_events += 1;
        }
    }
    assert_eq!(created, 1, "session.created once");
    assert_eq!(
        msg_events, 0,
        "D-FORK-1: no per-message/part SSE during fork"
    );
}

#[tokio::test]
async fn fork_at_message_is_exclusive_and_unknown_copies_all() {
    let dir = tempfile::tempdir().unwrap();
    let st = state(dir.path(), Payloads::default());
    let app = ocserve_http::router(st.clone());
    seed_session(&st, "ses_cut", "cut");
    seed_messages(&st, "ses_cut");

    // cut at msg_t3 → strictly BEFORE it → 2 messages (freeze slice(0, target))
    let (status, bytes) = fork(&app, "ses_cut", Some(r#"{"messageID":"msg_t3"}"#)).await;
    assert_eq!(
        status,
        StatusCode::OK,
        "{}",
        String::from_utf8_lossy(&bytes)
    );
    let dst = serde_json::from_slice::<Value>(&bytes).unwrap()["id"]
        .as_str()
        .unwrap()
        .to_string();
    assert_eq!(get_messages(&app, &dst).await.len(), 2, "exclusive cut");

    // unknown messageID → findIndex -1 → copy ALL (freeze quirk)
    let (status, bytes) = fork(&app, "ses_cut", Some(r#"{"messageID":"msg_nope"}"#)).await;
    assert_eq!(
        status,
        StatusCode::OK,
        "{}",
        String::from_utf8_lossy(&bytes)
    );
    let dst_all = serde_json::from_slice::<Value>(&bytes).unwrap()["id"]
        .as_str()
        .unwrap()
        .to_string();
    assert_eq!(
        get_messages(&app, &dst_all).await.len(),
        5,
        "unknown id copies all"
    );

    // empty body (NoContent arm) → also all
    let (status, bytes) = fork(&app, "ses_cut", None).await;
    assert_eq!(
        status,
        StatusCode::OK,
        "{}",
        String::from_utf8_lossy(&bytes)
    );
    let dst_empty = serde_json::from_slice::<Value>(&bytes).unwrap()["id"]
        .as_str()
        .unwrap()
        .to_string();
    assert_eq!(
        get_messages(&app, &dst_empty).await.len(),
        5,
        "NoContent copies all"
    );

    // cut at the FIRST message → empty fork
    let (status, bytes) = fork(&app, "ses_cut", Some(r#"{"messageID":"msg_t1"}"#)).await;
    assert_eq!(
        status,
        StatusCode::OK,
        "{}",
        String::from_utf8_lossy(&bytes)
    );
    let dst_zero = serde_json::from_slice::<Value>(&bytes).unwrap()["id"]
        .as_str()
        .unwrap()
        .to_string();
    assert_eq!(
        get_messages(&app, &dst_zero).await.len(),
        0,
        "cut at first → empty"
    );
}

#[tokio::test]
async fn fork_title_chains_like_freeze() {
    let dir = tempfile::tempdir().unwrap();
    let st = state(dir.path(), Payloads::default());
    let app = ocserve_http::router(st.clone());
    seed_session(&st, "ses_t", "X (fork #1)");
    seed_messages(&st, "ses_t");
    let (status, bytes) = fork(&app, "ses_t", Some("{}")).await;
    assert_eq!(status, StatusCode::OK);
    let v: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(v["title"], "X (fork #2)", "fork counter increments");
    // the pure helper, freeze-anchored
    assert_eq!(ocserve_http::forked_title("plain"), "plain (fork #1)");
    assert_eq!(ocserve_http::forked_title("X (fork #9)"), "X (fork #10)");
    assert_eq!(
        ocserve_http::forked_title("a (fork #1) (fork #2)"),
        "a (fork #1) (fork #3)",
        "greedy .+ → last occurrence"
    );
    assert_eq!(
        ocserve_http::forked_title("X (fork #abc)"),
        "X (fork #abc) (fork #1)",
        "non-digits do not match the regex"
    );
    assert_eq!(
        ocserve_http::forked_title(" (fork #1)"),
        " (fork #1) (fork #1)",
        "(.+) needs ≥1 char before the suffix"
    );
}

#[tokio::test]
async fn fork_error_envelopes_match_freeze() {
    let dir = tempfile::tempdir().unwrap();
    let st = state(dir.path(), Payloads::default());
    let app = ocserve_http::router(st.clone());

    // 404: NotFoundError envelope (same shape as GET /session/{id})
    let (status, bytes) = fork(&app, "ses_missing", Some("{}")).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let v: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(v["name"], "NotFoundError");
    assert!(
        v["data"]["message"]
            .as_str()
            .unwrap()
            .contains("ses_missing")
    );

    // 400 {"_tag":"BadRequest"} — payload decode failures (Effect decodes
    // before the handler → these precede even the 404)
    for bad in [
        "not json",
        "[1]",
        "42",
        "null",
        r#"{"messageID":42}"#,
        r#"{"messageID":null}"#,
    ] {
        let (status, bytes) = fork(&app, "ses_missing", Some(bad)).await;
        assert_eq!(
            status,
            StatusCode::BAD_REQUEST,
            "body {bad:?} → {}",
            String::from_utf8_lossy(&bytes)
        );
        let v: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(v, json!({"_tag": "BadRequest"}), "envelope for {bad:?}");
    }
}

#[tokio::test]
async fn fork_empty_source_yields_empty_session() {
    let dir = tempfile::tempdir().unwrap();
    let st = state(dir.path(), Payloads::default());
    let app = ocserve_http::router(st.clone());
    seed_session(&st, "ses_e", "empty");
    let (status, bytes) = fork(&app, "ses_e", Some("{}")).await;
    assert_eq!(
        status,
        StatusCode::OK,
        "{}",
        String::from_utf8_lossy(&bytes)
    );
    let v: Value = serde_json::from_slice(&bytes).unwrap();
    let dst = v["id"].as_str().unwrap();
    assert_eq!(v["title"], "empty (fork #1)");
    assert_eq!(get_messages(&app, dst).await.len(), 0);
}
