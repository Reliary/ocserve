//! W3 `POST /session/{id}/summarize` — probe-derived contract
//! (testdata/golden/summarize_contract.json): body `true`, empty user
//! marker + assistant summary, payload validation, busy semantics.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use refine_http::{AppState, LlmRegistry, Payloads, Wires};
use tower::ServiceExt;

fn state(dir: &std::path::Path) -> std::sync::Arc<AppState> {
    let db = refine_store::writer::db_path(dir);
    let writer = std::sync::Arc::new(refine_store::Writer::spawn(db.clone()).unwrap());
    let blobs = std::sync::Arc::new(refine_store::BlobStore::new(dir.join("blobs")).unwrap());
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
        Payloads::default(),
        Wires {
            db,
            blobs,
            writer,
            llm,
        },
    )
}

fn seed(st: &AppState, sid: &str) {
    refine_store::insert_session(
        &st.writer,
        &serde_json::json!({
            "id": sid, "projectID": "global", "directory": "/w", "path": sid,
            "slug": sid, "title": sid, "version": "1",
            "time": {"created": 1, "updated": 2},
        }),
    )
    .unwrap();
    refine_store::insert_message(
        &st.writer,
        Some(&*st.blobs),
        sid,
        &serde_json::json!({
            "id": "msg_seed_user", "sessionID": sid, "role": "user",
            "time": {"created": 100}, "agent": "plan",
        }),
        &[serde_json::json!({"id":"prt_s0","type":"text","text":"existing work"})],
    )
    .unwrap();
}

async fn post_sum(app: &axum::Router, sid: &str, body: serde_json::Value) -> (StatusCode, Vec<u8>) {
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/session/{sid}/summarize"))
                .header("content-type", "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
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

#[tokio::test]
async fn validation_404_and_busy_semantics() {
    let dir = tempfile::tempdir().unwrap();
    let st = state(dir.path());
    seed(&st, "ses_s");
    let app = refine_http::router(st.clone());

    // missing model fields →400 (payload validation BEFORE lock)
    let (s, b) = post_sum(&app, "ses_s", serde_json::json!({"providerID": "p"})).await;
    assert_eq!(s, 400, "body: {}", String::from_utf8_lossy(&b));
    assert!(String::from_utf8_lossy(&b).contains("providerID and modelID"));
    // unknown session →404 freeze envelope
    let (s, b) = post_sum(
        &app,
        "ses_nope",
        serde_json::json!({"providerID": "p", "modelID": "m"}),
    )
    .await;
    assert_eq!(s, 404);
    assert!(String::from_utf8_lossy(&b).contains("Session not found"));
    // busy →409 while a prompt holds the lock
    let arc = {
        let mut m = st.prompt_locks.lock();
        m.entry("ses_s".to_string())
            .or_insert_with(|| std::sync::Arc::new(tokio::sync::Mutex::new(())))
            .clone()
    };
    let holder = tokio::spawn({
        let a = arc.clone();
        async move {
            let _g = a.lock().await;
            tokio::time::sleep(std::time::Duration::from_secs(60)).await;
        }
    });
    tokio::time::sleep(std::time::Duration::from_millis(30)).await;
    let (s, b) = post_sum(
        &app,
        "ses_s",
        serde_json::json!({"providerID": "p", "modelID": "m"}),
    )
    .await;
    assert_eq!(s, 409, "body: {}", String::from_utf8_lossy(&b));
    holder.abort();
    st.prompt_locks.lock().remove("ses_s");
}

#[tokio::test]
async fn dead_provider_persists_anchor_and_summary_shell_then_fails() {
    let dir = tempfile::tempdir().unwrap();
    let st = state(dir.path());
    seed(&st, "ses_d");
    let app = refine_http::router(st.clone());
    let (s, b) = post_sum(
        &app,
        "ses_d",
        serde_json::json!({"providerID": "fake", "modelID": "m"}),
    )
    .await;
    assert_eq!(
        s,
        500,
        "provider failure surfaces: {}",
        String::from_utf8_lossy(&b)
    );
    // C5 source-derived shape (freeze tag v1.18.31, handlers/session.ts:273 +
    // compaction.ts:559-582): seed + ANCHOR (user with compaction part) +
    // summary shell (persisted before the provider turn failed). The old
    // "empty marker" recording was misattributed — re-verified at the tag.
    let msgs = refine_store::load_messages(&st.db, "ses_d", None).unwrap();
    assert_eq!(msgs.len(), 3, "seed + anchor + summary shell");
    let (info, parts) = &msgs[1];
    assert_eq!(info["role"], "user");
    assert_eq!(
        info["agent"], "plan",
        "agent = last user's agent (probe rule)"
    );
    assert_eq!(
        info["model"]["modelID"], "m",
        "anchor carries summarize model"
    );
    assert_eq!(parts.len(), 1, "anchor has exactly the compaction part");
    assert_eq!(parts[0]["type"], "compaction");
    assert_eq!(parts[0]["auto"], false, "manual flow = auto:false");
    // history instruction NOT persisted (prompt built transiently)
    let raw = serde_json::to_string(&msgs[1]).unwrap();
    assert!(
        !raw.contains("anchored summary"),
        "instruction stays transient"
    );
    // summary shell: linked to the anchor, never finished (stream failed)
    let (sinfo, sparts) = &msgs[2];
    assert_eq!(sinfo["role"], "assistant");
    assert_eq!(sinfo["summary"], true);
    assert_eq!(sinfo["parentID"], info["id"]);
    assert!(sinfo["finish"].is_null(), "shell never completed");
    assert_eq!(sparts.len(), 1);
    assert_eq!(sparts[0]["type"], "text");
    // projection linked (pending_anchor must NOT re-fire forever)
    let conn = refine_store::pragma::open_reader(&st.db).unwrap();
    let rows = refine_store::compaction_rows(&conn, "ses_d").unwrap();
    assert_eq!(rows.len(), 1, "projection row");
    assert!(rows[0].summary_msg_id.is_some(), "shell auto-linked");
}
