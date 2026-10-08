//! W1 `POST /session/search` — contract tests (PLAN §17 block): trigram
//! substring ≥3 chars, LIKE fallback ≤2, blob coverage, injection safety,
//! paging/truncated, snippet, reindex, cascade, backfill idempotence.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use ocserve_http::{AppState, LlmRegistry, Payloads, Wires};
use tower::ServiceExt;

fn state(dir: &std::path::Path) -> std::sync::Arc<AppState> {
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
        Payloads::default(),
        Wires {
            db,
            blobs,
            writer,
            llm,
        },
    )
}

fn tmp(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "ocserve-search-{}-{}-{name}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn seed(st: &AppState, sid: &str, mid: &str, texts: &[&str]) {
    ocserve_store::insert_session(
        &st.writer,
        &serde_json::json!({
            "id": sid, "projectID": "global", "directory": "/w", "path": sid,
            "slug": sid, "title": sid, "version": "1",
            "time": {"created": 1, "updated": 2},
        }),
    )
    .unwrap();
    let info = serde_json::json!({
        "id": mid, "sessionID": sid, "role": "assistant",
        "time": {"created": 1700000000000u64},
    });
    let parts: Vec<serde_json::Value> = texts
        .iter()
        .enumerate()
        .map(
            |(i, t)| serde_json::json!({"id": format!("prt_{mid}_{i}"), "type": "text", "text": t}),
        )
        .collect();
    ocserve_store::insert_message(&st.writer, Some(&*st.blobs), sid, &info, &parts).unwrap();
}

async fn post_search(
    app: &axum::Router,
    body: serde_json::Value,
) -> (StatusCode, serde_json::Value) {
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/session/search")
                .header("content-type", "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = resp.status();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    (status, serde_json::from_slice(&bytes).unwrap())
}

#[tokio::test]
async fn substring_case_scope_and_like_fallback() {
    let dir = tmp("core");
    let st = state(&dir);
    seed(
        &st,
        "ses_a",
        "msg_a1",
        &["The OCSERVE engine is fast today"],
    );
    seed(&st, "ses_b", "msg_b1", &["ocserve is lowercase here"]);

    let app = ocserve_http::router(st.clone());
    // mid-word ≥3 → trigram ("erve" spans OCSERVE/ocserve in both seeds)
    let (s, v) = post_search(&app, json_query("erve")).await;
    assert_eq!(s, 200);
    assert_eq!(v["hits"].as_array().unwrap().len(), 2, "both sessions: {v}");
    // case-insensitive
    let (_, v) = post_search(&app, json_query("OCSERVE")).await;
    assert_eq!(v["hits"].as_array().unwrap().len(), 2);
    // session scope
    let (_, v) = post_search(
        &app,
        serde_json::json!({"query": "ocserve", "sessionID": "ses_a"}),
    )
    .await;
    let hits = v["hits"].as_array().unwrap();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0]["sessionID"], "ses_a");
    // ≤2 chars → LIKE fallback (trigram can't serve this); "SE" also proves
    // the LIKE path is case-insensitive
    let (_, v) = post_search(&app, json_query("SE")).await;
    assert_eq!(v["hits"].as_array().unwrap().len(), 2, "LIKE path: {v}");
    // literal wildcard: '%' must not match everything
    let (s, v) = post_search(&app, json_query("%")).await;
    assert_eq!(s, 200);
    assert_eq!(
        v["hits"].as_array().unwrap().len(),
        0,
        "wildcards are literal"
    );
}

fn json_query(q: &str) -> serde_json::Value {
    serde_json::json!({"query": q})
}

#[tokio::test]
async fn injection_needles_neutralized() {
    let dir = tmp("inject");
    let st = state(&dir);
    seed(
        &st,
        "ses_i",
        "msg_i1",
        &["normal text with quotes \" and parens (1)"],
    );
    let app = ocserve_http::router(st.clone());
    for q in ["\"", "NEAR(", "a\" OR b", "x\" AND \"y", "*", "'] = {x} --"] {
        let (s, v) = post_search(&app, json_query(q)).await;
        assert_eq!(s, 200, "no parse error for {q:?}: {v}");
        assert!(v.get("hits").is_some(), "hits array present for {q:?}");
    }
    // a phrase that actually exists inside embedded quotes still hits
    let (_, v) = post_search(&app, json_query("parens")).await;
    assert_eq!(v["hits"].as_array().unwrap().len(), 1);
}

#[tokio::test]
async fn blobbed_parts_are_searchable() {
    let dir = tmp("blob");
    let st = state(&dir);
    let big = format!("start {} ZEBRACROSSING tail", "padding ".repeat(1500)); // >8KB → blob
    assert!(big.len() > ocserve_store::INLINE_PART_MAX);
    seed(&st, "ses_bl", "msg_bl1", &[&big]);
    let app = ocserve_http::router(st.clone());
    let (_, v) = post_search(&app, json_query("zebracrossing")).await;
    assert_eq!(
        v["hits"].as_array().unwrap().len(),
        1,
        "zstd-blobbed part must be searchable: {v}"
    );
}

#[tokio::test]
async fn paging_truncated_snippet_and_validation() {
    let dir = tmp("page");
    let st = state(&dir);
    seed(
        &st,
        "ses_p",
        "msg_p1",
        &[
            "hit one NEEDLE",
            "hit two NEEDLE",
            "hit three NEEDLE",
            "hit four NEEDLE",
            "hit five NEEDLE",
        ],
    );
    let app = ocserve_http::router(st.clone());
    // limit=2 of5 → truncated
    let (s, v) = post_search(&app, serde_json::json!({"query": "needle", "limit": 2})).await;
    assert_eq!(s, 200);
    assert_eq!(v["hits"].as_array().unwrap().len(), 2);
    assert_eq!(v["truncated"], true);
    // offset advances
    let (_, v) = post_search(
        &app,
        serde_json::json!({"query": "needle", "limit": 2, "offset": 2}),
    )
    .await;
    assert_eq!(v["hits"].as_array().unwrap().len(), 2);
    // validation: empty + oversized
    let (s, _) = post_search(&app, serde_json::json!({"query": "  "})).await;
    assert_eq!(s, 400, "empty query");
    let (s, _) = post_search(&app, json_query(&"x".repeat(513))).await;
    assert_eq!(s, 400, "oversized query");
    // snippet carries the needle
    let (_, v) = post_search(&app, serde_json::json!({"query": "needle", "limit": 1})).await;
    let snip = v["hits"][0]["snippet"].as_str().unwrap();
    assert!(snip.to_lowercase().contains("needle"), "snippet: {snip}");
}

#[tokio::test]
async fn update_part_reindexes_and_delete_cleans() {
    let dir = tmp("reindex");
    let st = state(&dir);
    seed(&st, "ses_r", "msg_r1", &["original ORIGWORD content"]);
    let app = ocserve_http::router(st.clone());
    let (_, v) = post_search(&app, json_query("origword")).await;
    assert_eq!(v["hits"].as_array().unwrap().len(), 1);

    // PATCH-shaped update through the store helper (route covered in golden)
    let part = serde_json::json!({
        "id": "prt_msg_r1_0", "sessionID": "ses_r", "messageID": "msg_r1",
        "type": "text", "text": "replaced ZULULWORD content"
    });
    assert!(
        ocserve_store::update_part(
            &st.writer,
            &st.blobs,
            "ses_r",
            "msg_r1",
            "prt_msg_r1_0",
            &part
        )
        .unwrap()
    );
    let (_, v) = post_search(&app, json_query("origword")).await;
    assert_eq!(v["hits"].as_array().unwrap().len(), 0, "old term gone");
    let (_, v) = post_search(&app, json_query("zululword")).await;
    assert_eq!(v["hits"].as_array().unwrap().len(), 1, "new term indexed");

    // cascade: delete the session → index rows gone
    st.writer
        .write(vec![ocserve_store::WriteOp::Sql {
            sql: "DELETE FROM session WHERE id='ses_r'".into(),
            params: vec![],
        }])
        .unwrap();
    let (_, v) = post_search(&app, json_query("zululword")).await;
    assert_eq!(
        v["hits"].as_array().unwrap().len(),
        0,
        "cascade cleaned index"
    );
}

#[tokio::test]
async fn backfill_indexes_raw_rows_idempotently() {
    let dir = tmp("backfill");
    let st = state(&dir);
    seed(&st, "ses_bf", "msg_bf1", &["seed part"]);
    // a part that exists WITHOUT its projection (simulates pre-v7 rows)
    st.writer
        .write(vec![ocserve_store::WriteOp::Sql {
            sql: "INSERT INTO msg_part (id, message_id, session_id, seq, type, byte_len, inline, blob_sha) VALUES ('prt_raw','msg_bf1','ses_bf',99,'text',30,'{\"type\":\"text\",\"text\":\"raw INDEXME row\"}',NULL)".into(),
            params: vec![],
        }])
        .unwrap();
    let app = ocserve_http::router(st.clone());
    let (_, v) = post_search(&app, json_query("indexme")).await;
    assert_eq!(v["hits"].as_array().unwrap().len(), 0, "not indexed yet");

    let (idx, skip, _ms) = ocserve_store::backfill_part_search(&st.writer, &st.db).unwrap();
    // seed part was projected AT insert time (insert_message companion) —
    // only the raw pre-projection row needs backfilling
    assert_eq!(idx, 1, "only the raw row needs indexing (skip={skip})");
    let (_, v) = post_search(&app, json_query("indexme")).await;
    assert_eq!(v["hits"].as_array().unwrap().len(), 1, "raw part now found");
    // idempotent: nothing left to do
    let (idx2, _, _) = ocserve_store::backfill_part_search(&st.writer, &st.db).unwrap();
    assert_eq!(idx2, 0, "second run is a no-op");
}
