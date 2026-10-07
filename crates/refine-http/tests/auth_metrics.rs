//! N6 / PLAN §3: auth decision table + runtime route key-shape tests.
//! Decision table (TESTING §4): auth_mode × credential presence → status.
//! Live freeze is passwordless (recorded), so `off` is the default world;
//! `basic` must fail closed on EVERY route including SSE and /metrics.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use tower::ServiceExt;

fn app_off() -> axum::Router {
    refine_http::router(refine_http::AppState::new())
}

fn app_basic() -> axum::Router {
    refine_http::router(refine_http::AppState::with_auth(Some((
        "u".into(),
        "p".into(),
    ))))
}

const ROUTES: &[&str] = &[
    "/global/health",
    "/config",
    "/agent",
    "/path",
    "/project",
    "/project/current",
    "/session",
    "/session/status",
    "/global/event",
    "/metrics",
];

async fn status_of(app: axum::Router, uri: &str, auth: Option<&str>) -> StatusCode {
    let mut builder = Request::builder().uri(uri);
    if let Some(a) = auth {
        builder = builder.header(axum::http::header::AUTHORIZATION, a);
    }
    let resp = app
        .oneshot(builder.body(Body::empty()).unwrap())
        .await
        .unwrap();
    resp.status()
}

/// Row: auth=off, no credentials → 200 everywhere (the freeze world).
#[tokio::test]
async fn auth_off_allows_everything() {
    for uri in ROUTES {
        let s = status_of(app_off(), uri, None).await;
        assert_eq!(s, StatusCode::OK, "auth=off must allow {uri}");
    }
}

/// Row: auth=basic, no credentials → 401 everywhere (fail closed, incl. SSE+metrics).
#[tokio::test]
async fn auth_basic_denies_without_credentials() {
    for uri in ROUTES {
        let s = status_of(app_basic(), uri, None).await;
        assert_eq!(s, StatusCode::UNAUTHORIZED, "auth=basic must 401 {uri}");
    }
}

/// Row: auth=basic, wrong credentials → 401.
#[tokio::test]
async fn auth_basic_denies_wrong_credentials() {
    use base64::Engine;
    let bad = base64::engine::general_purpose::STANDARD.encode("u:wrong");
    let s = status_of(app_basic(), "/global/health", Some(&format!("Basic {bad}"))).await;
    assert_eq!(s, StatusCode::UNAUTHORIZED);
    let s = status_of(app_basic(), "/session", Some("Bearer xyz")).await;
    assert_eq!(s, StatusCode::UNAUTHORIZED, "non-Basic scheme denied");
}

/// Row: auth=basic, correct credentials → 200 everywhere.
#[tokio::test]
async fn auth_basic_allows_correct_credentials() {
    use base64::Engine;
    let good = base64::engine::general_purpose::STANDARD.encode("u:p");
    for uri in ROUTES {
        let s = status_of(app_basic(), uri, Some(&format!("Basic {good}"))).await;
        assert_eq!(s, StatusCode::OK, "valid credentials must allow {uri}");
    }
}

/// /path key shape vs recorded golden projection (values are env-dependent,
/// keys are the contract — manifest mode=keys).
#[tokio::test]
async fn path_keys_match_golden() {
    let resp = app_off()
        .oneshot(Request::builder().uri("/path").body(Body::empty()).unwrap())
        .await
        .unwrap();
    let body = resp.into_body().collect().await.unwrap().to_bytes();
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
    let mut got = std::collections::BTreeSet::new();
    refine_http::keypaths(&v, "$", &mut got);
    let want: std::collections::BTreeSet<String> =
        ["$.config", "$.directory", "$.home", "$.state", "$.worktree"]
            .iter()
            .map(|s| s.to_string())
            .collect();
    assert_eq!(got, want, "/path key shape drifted");
    // no empty strings — boot must have derived real values
    for k in ["home", "state", "config", "worktree", "directory"] {
        assert!(!v[k].as_str().unwrap_or("").is_empty(), "/path.{k} empty");
    }
}

/// /project (array) and /project/current (object) key shapes vs golden.
#[tokio::test]
async fn project_keys_match_golden() {
    for (uri, id_key) in [("/project", "$[].id"), ("/project/current", "$.id")] {
        let resp = app_off()
            .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
            .await
            .unwrap();
        let body = resp.into_body().collect().await.unwrap().to_bytes();
        let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
        let mut got = std::collections::BTreeSet::new();
        refine_http::keypaths(&v, "$", &mut got);
        let wt_key = id_key.replace(".id", ".worktree");
        let t_key = id_key.replace(".id", ".time.created");
        let sb_key = id_key.replace(".id", ".sandboxes");
        for want in [id_key, wt_key.as_str(), t_key.as_str(), sb_key.as_str()] {
            assert!(got.contains(want), "{uri} missing {want}; got {got:?}");
        }
    }
}

/// /metrics exposes rss series in Prometheus text format (SRE §2).
#[tokio::test]
async fn metrics_exposes_kpi_series() {
    let resp = app_off()
        .oneshot(
            Request::builder()
                .uri("/metrics")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let ctype = resp
        .headers()
        .get(axum::http::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_string();
    assert!(
        ctype.starts_with("text/plain"),
        "prometheus content type, got {ctype}"
    );
    let body = resp.into_body().collect().await.unwrap().to_bytes();
    let text = String::from_utf8_lossy(&body);
    assert!(text.contains("refine_requests_total"), "counter missing");
    assert!(text.contains("refine_rss_bytes"), "rss gauge missing");
    // L1 splice + memo activity must be observable in production, not only in
    // a bench run (the lib.rs doc comment promised a metric that had no
    // caller until this was wired — found during the 2026-10-07 deploy).
    for want in [
        "refine_splice_rows_total",
        "refine_splice_fallbacks_total",
        "refine_memo_search_hits",
        "refine_memo_search_misses",
        "refine_memo_list_hits",
        "refine_memo_list_misses",
        "refine_memo_search_entries",
    ] {
        assert!(text.contains(want), "{want} missing from /metrics");
    }
    // every emitted series parses as `name value` with a numeric value
    for line in text.lines() {
        if line.starts_with('#') || line.is_empty() {
            continue;
        }
        let mut parts = line.split_whitespace();
        let (Some(name), Some(val)) = (parts.next(), parts.next()) else {
            continue;
        };
        if name.starts_with("refine_splice") || name.starts_with("refine_memo") {
            assert!(
                val.parse::<f64>().is_ok(),
                "{name} has non-numeric value {val:?}"
            );
        }
    }
}
