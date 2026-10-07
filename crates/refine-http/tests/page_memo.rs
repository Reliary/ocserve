//! PERF-10X F9 — paged `/message` wire memo, integration level.
//!
//! Own PROCESS (page memo state is process-global; env flip serialized
//! behind LOCK — precedent f7_memo.rs / reader_accounting.rs).
//! Contract under test: hits are epoch-exact (a raw no-bump write stays
//! invisible; a writer write invalidates), the kill switch bypasses, and
//! byte-parity with the streaming path is covered by golden/paging/sync.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use refine_http::{AppState, LlmRegistry, Payloads, Wires};
use std::sync::{Mutex, MutexGuard, OnceLock};
use tower::ServiceExt;

static LOCK: OnceLock<Mutex<()>> = OnceLock::new();

fn lock() -> MutexGuard<'static, ()> {
    LOCK.get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|e| e.into_inner())
}

fn tmp(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "refine-pagememo-{}-{}-{name}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

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

/// Writer-path session (bumps the write epoch — established state).
fn seed_session(st: &AppState, sid: &str) {
    refine_store::insert_session(
        &st.writer,
        &serde_json::json!({
            "id": sid, "projectID": "global", "directory": "/w", "path": sid,
            "slug": sid, "title": sid, "version": "1",
            "time": {"created": 1, "updated": 2},
        }),
    )
    .unwrap();
}

fn seed_msg(st: &AppState, sid: &str, mid: &str, t_ms: u64) {
    let info = serde_json::json!({
        "id": mid, "sessionID": sid, "role": "user",
        "time": {"created": t_ms},
    });
    let parts = vec![serde_json::json!({
        "id": format!("prt_{mid}"), "type": "text", "text": mid
    })];
    refine_store::insert_message(&st.writer, Some(&*st.blobs), sid, &info, &parts).unwrap();
}

async fn get_page(app: &axum::Router, path: &str) -> (StatusCode, Vec<u8>, u64) {
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("GET")
                .uri(path)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let status = resp.status();
    let n = resp
        .headers()
        .get("content-length")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(0);
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    (status, bytes.to_vec(), n)
}

fn count_msgs(b: &[u8]) -> usize {
    serde_json::from_slice::<serde_json::Value>(b)
        .unwrap()
        .as_array()
        .map(|a| a.len())
        .unwrap_or(0)
}

/// The epoch contract: memo hit hides a raw (no-funnel) write; a
/// writer-funnel write invalidates and the fresh read sees BOTH writes.
#[tokio::test]
async fn page_memo_epoch_exact() {
    let _g = lock();
    let dir = tmp("epoch");
    let st = state(&dir);
    seed_session(&st, "ses_pm");
    seed_msg(&st, "ses_pm", "m_a", 1700000000001);
    seed_msg(&st, "ses_pm", "m_b", 1700000000002);
    let app = refine_http::router(st.clone());

    let (s, b1, _) = get_page(&app, "/session/ses_pm/message?limit=50").await;
    assert_eq!(s, 200);
    assert_eq!(count_msgs(&b1), 2, "seeded page");

    // raw no-funnel write: same db file, NO epoch bump
    {
        let conn = refine_store::pragma::open_writer(&refine_store::writer::db_path(&dir)).unwrap();
        conn.execute(
            "INSERT INTO msg (id, session_id, role, seq, time_created, info) VALUES ('m_raw','ses_pm','user',3,1700000000003,'{}')",
            [],
        )
        .unwrap();
    }
    let (s, b2, _) = get_page(&app, "/session/ses_pm/message?limit=50").await;
    assert_eq!(s, 200);
    assert_eq!(
        count_msgs(&b2),
        2,
        "raw write must stay invisible — memo hit (cache engaged)"
    );
    assert_eq!(b2, b1, "hit serves byte-identical body");

    // writer-funnel write: epoch bumps, fresh read sees raw + writer rows
    seed_msg(&st, "ses_pm", "m_c", 1700000000004);
    let (s, b3, _) = get_page(&app, "/session/ses_pm/message?limit=50").await;
    assert_eq!(s, 200);
    assert_eq!(
        count_msgs(&b3),
        4,
        "writer write invalidates — fresh read sees raw + writer rows"
    );
    assert_ne!(b3, b1, "invalidated body differs");

    let (hits, misses, entries) = refine_http::page_memo_stats();
    assert!(hits >= 1, "at least one memo hit recorded: {hits}");
    assert!(misses >= 1, "at least one miss recorded: {misses}");
    assert!(entries >= 1, "entry stored: {entries}");
    let _ = std::fs::remove_dir_all(&dir);
}

/// Kill switch: REFINE_PAGE_MEMO=0 serves every request straight from the
/// store (raw writes visible immediately).
#[tokio::test]
async fn page_memo_env_off_bypasses() {
    let _g = lock();
    // SAFETY: LOCK serializes every test in this process — no sibling
    // thread reads env during the flip (edition-2024 rule, f7 precedent).
    unsafe {
        std::env::set_var("REFINE_PAGE_MEMO", "0");
    }
    assert!(!refine_http::page_memo_enabled());

    let dir = tmp("envoff");
    let st = state(&dir);
    seed_session(&st, "ses_pm2");
    seed_msg(&st, "ses_pm2", "m_a", 1700000000001);
    let app = refine_http::router(st.clone());
    let (s, b1, _) = get_page(&app, "/session/ses_pm2/message?limit=50").await;
    assert_eq!(s, 200);
    assert_eq!(count_msgs(&b1), 1);

    {
        let conn = refine_store::pragma::open_writer(&refine_store::writer::db_path(&dir)).unwrap();
        conn.execute(
            "INSERT INTO msg (id, session_id, role, seq, time_created, info) VALUES ('m_raw','ses_pm2','user',2,1700000000002,'{}')",
            [],
        )
        .unwrap();
    }
    let (s, b2, cl) = get_page(&app, "/session/ses_pm2/message?limit=50").await;
    assert_eq!(s, 200);
    assert_eq!(
        count_msgs(&b2),
        2,
        "memo off => raw write visible on the very next read"
    );
    assert_eq!(cl, 0, "off path streams (no Content-Length)");

    unsafe {
        std::env::remove_var("REFINE_PAGE_MEMO");
    }
    assert!(refine_http::page_memo_enabled());
    let _ = std::fs::remove_dir_all(&dir);
}
