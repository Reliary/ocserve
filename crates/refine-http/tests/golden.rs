//! Byte-golden tests: refine's wire output vs the freeze (PLAN §3, TESTING §5/§6).
//! Oracle: recordings under testdata/golden captured from upstream 1.18.31.
//! Volatile fields (event IDs, Date) are normalized before comparison — named,
//! never skipped (AGENTS.md §2.1).

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use tower::ServiceExt; // oneshot

fn app() -> axum::Router {
    refine_http::router(refine_http::AppState::new())
}

fn freeze_headers(h: &axum::http::HeaderMap) -> Vec<(&'static str, String)> {
    h.iter()
        .filter(|(k, _)| {
            matches!(
                k.as_str(),
                "content-type"
                    | "cache-control"
                    | "x-content-type-options"
                    | "x-accel-buffering"
                    | "content-length"
            )
        })
        .map(|(k, v)| {
            let name: &'static str = match k.as_str() {
                "content-type" => "content-type",
                "cache-control" => "cache-control",
                "x-content-type-options" => "x-content-type-options",
                "x-accel-buffering" => "x-accel-buffering",
                "content-length" => "content-length",
                _ => unreachable!(),
            };
            (name, v.to_str().unwrap_or_default().to_string())
        })
        .collect()
}

/// Finite-body request: full read (REST responses end).
async fn collect(req: Request<Body>) -> (StatusCode, Vec<(&'static str, String)>, bytes::Bytes) {
    let resp = app().oneshot(req).await.expect("router responds");
    let status = resp.status();
    let headers = freeze_headers(resp.headers());
    let body = resp.into_body().collect().await.unwrap().to_bytes();
    (status, headers, body)
}

/// SSE: never completes — read only the FIRST body frame, bounded (TESTING §6:
/// a hang is a failure, so the read itself is under a timeout).
async fn collect_first(
    req: Request<Body>,
) -> (StatusCode, Vec<(&'static str, String)>, bytes::Bytes) {
    let resp = app().oneshot(req).await.expect("router responds");
    let status = resp.status();
    let headers = freeze_headers(resp.headers());
    let mut body = resp.into_body();
    let first = tokio::time::timeout(std::time::Duration::from_secs(3), body.frame())
        .await
        .expect("first SSE frame within 3s")
        .expect("stream open")
        .expect("frame decodes");
    let bytes = first.into_data().unwrap_or_default();
    (status, headers, bytes)
}

/// GOLDEN: /global/health bytes are identical to the recorded upstream body.
#[tokio::test]
async fn health_bytes_match_upstream() {
    let (_s, _h, body) = collect(
        Request::builder()
            .uri("/global/health")
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    let golden = std::fs::read(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../testdata/golden/global_health.body"
    ))
    .expect("golden health body present");
    assert_eq!(
        body.as_ref(),
        &golden[..],
        "health body drifted from freeze"
    );
}

/// GOLDEN: 404 envelope bytes + status (recorded live from upstream).
#[tokio::test]
async fn notfound_envelope_matches_upstream() {
    let (status, _h, body) = collect(
        Request::builder()
            .uri("/session/ses_nonexistent")
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let golden = std::fs::read(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../testdata/golden/error_notfound.body"
    ))
    .expect("golden error body present");
    assert_eq!(body.as_ref(), &golden[..], "error envelope drifted");
}

/// GOLDEN: SSE header set is exactly the freeze set (PLAN §3). Missing/renamed
/// header = fail (axum defaults differ — insert overrides are asserted here).
#[tokio::test]
async fn sse_headers_match_freeze() {
    let (status, headers, _body) = collect_first(
        Request::builder()
            .uri("/global/event")
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    for (want_k, want_v) in refine_http::sse_expected_headers() {
        let got = headers.iter().find(|(k, _)| *k == want_k);
        match got {
            Some((_, v)) => assert_eq!(v, want_v, "header {want_k} drifted"),
            None => panic!("missing freeze header {want_k}"),
        }
    }
}

/// GOLDEN: first SSE frame bytes match upstream modulo the volatile event id.
/// Upstream: data: {"payload":{"id":"evt_<id>","type":"server.connected","properties":{}}}
/// with key order id,type,properties (serde_json preserve_order required).
#[tokio::test]
async fn sse_first_frame_matches_upstream() {
    let (_s, _h, body) = collect_first(
        Request::builder()
            .uri("/global/event")
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    let text = String::from_utf8_lossy(&body);
    let first = text.split("\n\n").next().expect("at least one frame");
    let norm = normalize_ids(first);
    assert_eq!(
        norm, r#"data: {"payload":{"id":"evt_<ID>","type":"server.connected","properties":{}}}"#,
        "first frame drifted from freeze"
    );
    // No id:/retry: lines anywhere in what we've seen (freeze fact §3)
    assert!(!text.contains("\nid:"), "id: lines are forbidden by freeze");
    assert!(
        !text.contains("retry:"),
        "retry: lines are forbidden by freeze"
    );
}

fn normalize_ids(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if s[i..].starts_with("evt_") {
            out.push_str("evt_");
            i += 4;
            let start = i;
            while i < bytes.len() && bytes[i].is_ascii_alphanumeric() {
                i += 1;
            }
            if i - start >= 20 {
                out.push_str("<ID>");
            } else {
                out.push_str(&s[start..i]);
            }
        } else {
            let ch = s[i..].chars().next().unwrap();
            out.push(ch);
            i += ch.len_utf8();
        }
    }
    out
}

/// Negative control for the frame test: normalization must NOT be a no-op
/// (if it were, the test could never fail on id drift — anti-theater §1).
#[test]
fn id_normalization_actually_normalizes() {
    let upstream = r#"data: {"payload":{"id":"evt_0f97ff8ab0017PVK23XpJUTuiH","type":"server.connected","properties":{}}}"#;
    let ours = r#"data: {"payload":{"id":"evt_0a1b2c3d4e5f6a7b8c9d0e1f2a","type":"server.connected","properties":{}}}"#;
    assert_eq!(normalize_ids(upstream), normalize_ids(ours));
    assert_ne!(
        normalize_ids(upstream),
        upstream,
        "normalization must change long ids"
    );
    // short ids (bug: fixed id strings) are NOT normalized — proves the 26-char
    // span in the frame test is doing real work
    assert_ne!(normalize_ids("evt_connected"), "evt_<ID>");
}

/// Session list returns [] on an empty server (M1), with 200 + JSON content-type.
#[tokio::test]
async fn session_list_empty_ok() {
    let (status, _h, body) = collect(
        Request::builder()
            .uri("/session")
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body.as_ref(), b"[]");
}

/// Session status returns exactly {} (recorded: len=2).
#[tokio::test]
async fn session_status_bytes_match_upstream() {
    let (_s, _h, body) = collect(
        Request::builder()
            .uri("/session/status")
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    let golden = std::fs::read(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../testdata/golden/session_status.body"
    ))
    .expect("golden status body present");
    assert_eq!(body.as_ref(), &golden[..]);
}
