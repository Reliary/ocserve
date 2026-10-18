//! N6 / PLAN §3: auth decision table + runtime route key-shape tests.
//! Decision table (TESTING §4): auth_mode × credential presence → status.
//! Live freeze is passwordless (recorded), so `off` is the default world;
//! `basic` must fail closed on EVERY route including SSE and /metrics.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use tower::ServiceExt;

fn app_off() -> axum::Router {
    ocserve_http::router(ocserve_http::AppState::new())
}

fn app_basic() -> axum::Router {
    ocserve_http::router(ocserve_http::AppState::with_auth(Some((
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

/// Byte-exact upstream challenge (probed live on 2.0.28): 401 +
/// `www-authenticate: Basic realm="Secure Area"` +
/// `{"_tag":"UnauthorizedError","message":"Authentication required"}`.
#[tokio::test]
async fn auth_challenge_is_byte_exact() {
    let resp = app_basic()
        .oneshot(
            Request::builder()
                .uri("/api/info")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    let challenge = resp
        .headers()
        .get(axum::http::header::WWW_AUTHENTICATE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_string();
    assert_eq!(
        challenge, "Basic realm=\"Secure Area\"",
        "challenge drifted"
    );
    let body = resp.into_body().collect().await.unwrap().to_bytes();
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(v["_tag"], "UnauthorizedError");
    assert_eq!(v["message"], "Authentication required");
    // CONTROL: challenge without the header → browsers never prompt, red.
}

/// Absent and wrong credentials are indistinguishable (no oracle for
/// attackers to distinguish "no creds" from "bad creds").
#[tokio::test]
async fn auth_absent_and_wrong_are_indistinguishable() {
    use base64::Engine;
    let bad = base64::engine::general_purpose::STANDARD.encode("u:wrong");
    let plain = app_basic()
        .oneshot(
            Request::builder()
                .uri("/api/info")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let denied = app_basic()
        .oneshot(
            Request::builder()
                .uri("/api/info")
                .header(axum::http::header::AUTHORIZATION, format!("Basic {bad}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(plain.status(), denied.status());
    let pb = plain.into_body().collect().await.unwrap().to_bytes();
    let db = denied.into_body().collect().await.unwrap().to_bytes();
    assert_eq!(pb, db, "absent vs wrong must be byte-identical");
    // CONTROL: distinct messages per case → comparison fails, red.
}

/// Unknown paths stay public under auth (upstream parity, probed live:
/// `/nonexistent-path-xyz` → 200 UI HTML). The fallback is deliberately
/// outside the auth layer — the UI must load before it can prompt.
#[tokio::test]
async fn auth_unknown_paths_stay_public() {
    for uri in ["/nonexistent-path-xyz", "/"] {
        let resp = app_basic()
            .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK, "{uri} must stay public");
    }
    // CONTROL: fallback moved inside auth → 401 here, red (parity break).
}

/// Wrong-method on a bound API route (probed live on 2.0.28): 401
/// unauthenticated, 404 authenticated — never the SPA. (v1 freeze serves
/// the SPA here; `/api/*` is the v2 surface and follows v2.)
#[tokio::test]
async fn auth_wrong_method_matches_v2() {
    use base64::Engine;
    let denied = app_basic()
        .oneshot(
            Request::builder()
                .method("PATCH")
                .uri("/api/info")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(denied.status(), StatusCode::UNAUTHORIZED);
    let good = base64::engine::general_purpose::STANDARD.encode("u:p");
    let nf = app_basic()
        .oneshot(
            Request::builder()
                .method("PATCH")
                .uri("/api/info")
                .header(axum::http::header::AUTHORIZATION, format!("Basic {good}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(nf.status(), StatusCode::NOT_FOUND);
    // CONTROL: SPA fallback on /api wrong-method → 200 here, red.
}

/// F7 (security/reliability review): an UNBOUND `/api/*` GET must be a JSON
/// 404, never the SPA index HTML (serving HTML 200 to an API client crashes the
/// SDK's shape parse). Non-API unknown paths stay public UI (upstream parity).
#[tokio::test]
async fn unbound_api_path_is_json_404_not_html() {
    let resp = app_off()
        .oneshot(
            Request::builder()
                .uri("/api/definitely-not-a-bound-route")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    let ct = resp
        .headers()
        .get(axum::http::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    assert!(
        ct.contains("application/json"),
        "unbound /api/* must be JSON, got content-type {ct}"
    );
    // CONTROL: removing the /api guard in ui_fallback → text/html, red.
}

/// CORS preflight through the PRODUCTION stack (perimeter layer outside the
/// router, as serve() wires it): 204 on both routes, off and basic. The
/// perimeter exists because fallback + MNF responses bypass router middleware
/// in our builder order (proven empirically 2026-10-17: preflight resolved to
/// ui_fallback/MNF with no 204 and no ACAO while routed GETs carried ACAO).
/// A router-direct assertion pins the bypass so a future reorder restoring
/// in-router coverage shows up as a changed test, not silent drift.
#[tokio::test]
async fn auth_cors_preflight_answers_without_credentials() {
    for (label, app) in [("off", app_off()), ("basic", app_basic())] {
        // Production shape: perimeter outside router (layer AFTER build).
        let stack = app
            .clone()
            .layer(axum::middleware::from_fn(ocserve_http::ui::perimeter_cors));
        for uri in ["/global/health", "/api/info"] {
            let mut b = Request::builder().method("OPTIONS").uri(uri);
            b = b
                .header("Origin", "http://localhost:3000")
                .header("Access-Control-Request-Method", "POST");
            if label == "basic" {
                b = b.header("Authorization", "Basic b3BlbmNvZGU6eHh4");
            }
            let resp = stack
                .clone()
                .oneshot(b.body(Body::empty()).unwrap())
                .await
                .unwrap();
            assert_eq!(
                resp.status(),
                StatusCode::NO_CONTENT,
                "{label} preflight {uri}"
            );
            assert_eq!(
                resp.headers()
                    .get("access-control-allow-origin")
                    .map(|v| v.to_str().unwrap().to_string()),
                Some("http://localhost:3000".to_string()),
                "{label} preflight ACAO {uri}"
            );
        }
        // Router-direct pins the bypass (informational): /api/info OPTIONS
        // resolves to the MNF (inline auth check), never to the gate.
        let mut b = Request::builder().method("OPTIONS").uri("/api/info");
        b = b
            .header("Origin", "http://localhost:3000")
            .header("Access-Control-Request-Method", "POST");
        let resp = app
            .clone()
            .oneshot(b.body(Body::empty()).unwrap())
            .await
            .unwrap();
        if label == "off" {
            assert_eq!(
                resp.status(),
                StatusCode::NOT_FOUND,
                "router-direct MNF 404"
            );
        } else {
            assert_eq!(
                resp.status(),
                StatusCode::UNAUTHORIZED,
                "router-direct MNF challenges"
            );
        }
    }
}

/// Edge credentials: user mismatch, empty, non-UTF8, malformed base64 —
/// all deny without panic or oracle.
#[tokio::test]
async fn auth_edge_credentials_deny() {
    use base64::Engine;
    let cases = vec![
        format!(
            "Basic {}",
            base64::engine::general_purpose::STANDARD.encode("other:p")
        ),
        format!(
            "Basic {}",
            base64::engine::general_purpose::STANDARD.encode("u:")
        ),
        "Basic !!!not-base64!!!".to_string(),
        "Bearer u:p".to_string(),
        "Basic".to_string(),
    ];
    for cred in cases {
        let s = status_of(app_basic(), "/api/info", Some(&cred)).await;
        assert_eq!(
            s,
            StatusCode::UNAUTHORIZED,
            "edge credential allowed: {cred:?}"
        );
    }
    // CONTROL: any allow → red.
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
    ocserve_http::keypaths(&v, "$", &mut got);
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
        ocserve_http::keypaths(&v, "$", &mut got);
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
    assert!(text.contains("ocserve_requests_total"), "counter missing");
    assert!(text.contains("ocserve_rss_bytes"), "rss gauge missing");
    // L1 splice + memo activity must be observable in production, not only in
    // a bench run (the lib.rs doc comment promised a metric that had no
    // caller until this was wired — found during the 2026-10-07 deploy).
    for want in [
        "ocserve_splice_rows_total",
        "ocserve_splice_fallbacks_total",
        "ocserve_memo_search_hits",
        "ocserve_memo_search_misses",
        "ocserve_memo_list_hits",
        "ocserve_memo_list_misses",
        "ocserve_memo_search_entries",
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
        if name.starts_with("ocserve_splice") || name.starts_with("ocserve_memo") {
            assert!(
                val.parse::<f64>().is_ok(),
                "{name} has non-numeric value {val:?}"
            );
        }
    }
}
