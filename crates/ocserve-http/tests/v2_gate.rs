//! The `OCSERVE_V2` flag must mean exactly one thing.
//!
//! Audit 2026-10-20: with the flag unset, the live server still answered 32
//! v2-only operations (`GET /api/config`, `/api/config/shell`,
//! `/api/debug/location`, `/api/experimental/migration/v1`, ...). The flag was
//! documented as "the whole v2 runtime stays dark until acceptance", so the
//! half-on state was an untested configuration with no oracle.
//!
//! The gate surface is spec-derived: `(2.0.28 operations) - (1.18.31
//! operations)`. This test pins both halves of that boundary, because gating
//! too much is as wrong as gating too little: the frozen 1.18.31 contract
//! contains 58 `/api/*` operations and the 1.18.31 TUI calls 18 `client.v2.*`
//! methods on mount.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use tower::ServiceExt;

const TABLE: &str = include_str!("../assets/openapi/v2-only.tsv");

fn table() -> Vec<(String, String)> {
    TABLE
        .lines()
        .filter(|l| !l.starts_with('#') && !l.trim().is_empty())
        .filter_map(|l| l.split_once('\t'))
        .map(|(m, p)| (m.to_string(), p.to_string()))
        .collect()
}

fn app_with_v2(v2: bool) -> axum::Router {
    ocserve_http::router(ocserve_http::AppState::with_payloads_and_v2(
        None,
        Default::default(),
        v2,
    ))
}

async fn status(app: &axum::Router, method: &str, uri: &str) -> (StatusCode, String) {
    let req = Request::builder()
        .method(method)
        .uri(uri)
        .body(Body::empty())
        .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    let status = resp.status();
    let ct = resp
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    let body = resp.into_body().collect().await.unwrap().to_bytes();
    (status, format!("{ct} {}", String::from_utf8_lossy(&body)))
}

fn concrete(path: &str) -> String {
    path.replace("{*path}", "x")
        .replace("{sessionID}", "ses_missing")
        .replace("{messageID}", "msg_missing")
        .replace("{partID}", "prt_missing")
        .replace("{agentID}", "agent_missing")
        .replace("{providerID}", "provider_missing")
        .replace("{integrationID}", "integration_missing")
        .replace("{attemptID}", "attempt_missing")
        .replace("{credentialID}", "cred_missing")
        .replace("{ptyID}", "pty_missing")
        .replace("{requestID}", "per_missing")
        .replace("{formID}", "form_missing")
        .replace("{inboxID}", "inbox_missing")
        .replace("{id}", "missing")
        .replace("{key}", "missing")
        .replace("{server}", "missing")
        .replace("{code}", "missing")
        .replace("{projectID}", "project_missing")
        .replace("{sessionId}", "ses_missing")
}

/// With the flag off, every v2-only operation is a clean JSON 404.
#[tokio::test]
async fn flag_off_every_v2_only_operation_is_404() {
    let app = app_with_v2(false);
    let mut leaked = Vec::new();
    for (method, path) in table() {
        let uri = concrete(&path);
        let (status, body) = status(&app, &method, &uri).await;
        if status != StatusCode::NOT_FOUND {
            leaked.push(format!("{method} {uri} -> {status}"));
        } else if !body.contains("application/json") || !body.contains("NotFoundError") {
            leaked.push(format!("{method} {uri} -> 404 but body {body}"));
        }
    }
    assert!(
        leaked.is_empty(),
        "v2-only operations answered with OCSERVE_V2 unset:\n  {}",
        leaked.join("\n  ")
    );
}

/// The gate must not swallow the frozen v1 contract. These are `/api/*`
/// operations that exist in 1.18.31 and are called by the 1.18.31 TUI's
/// `client.v2.*` DataProvider; they must keep answering with the flag off.
#[tokio::test]
async fn flag_off_keeps_the_frozen_v1_api_surface() {
    let app = app_with_v2(false);
    for uri in [
        "/api/health",
        "/api/location",
        "/api/agent",
        "/api/model",
        "/api/provider",
        "/api/skill",
        "/api/command",
        "/api/integration",
        "/api/reference",
        "/api/fs/list?path=/tmp",
        "/api/session",
        "/api/session/active",
    ] {
        let (status, body) = status(&app, "GET", uri).await;
        // The property under test is "the route is still bound", not "the
        // handler is happy with an empty fixture" (fs/list legitimately 400s
        // on a path outside the project). A gated route would be 404.
        assert_ne!(
            status,
            StatusCode::NOT_FOUND,
            "{uri} must stay served: {body}"
        );
        assert!(
            !body.starts_with("text/html"),
            "{uri} must not fall through to the SPA, got {body}"
        );
    }
}

/// The table must stay in sync with the vendored contracts. A future upstream
/// bump that adds a v2-only operation without regenerating this file would
/// silently reopen the leak, so the boundary is asserted here rather than
/// trusted.
#[test]
fn gate_table_matches_the_vendored_contract_difference() {
    // Read the sibling crate's vendored contracts from the workspace rather
    // than `include_str!`: embedding them would pull files from outside this
    // crate's root, which packaging cannot see (guard rule 30). Tests are not
    // run from a published `.crate`, so a workspace-relative read is correct
    // here and CI always runs in the workspace.
    let specs =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../ocserve-core/assets/openapi");
    let read = |name: &str| -> serde_json::Value {
        let path = specs.join(name);
        serde_json::from_str(
            &std::fs::read_to_string(&path)
                .unwrap_or_else(|e| panic!("read {}: {e}", path.display())),
        )
        .unwrap_or_else(|e| panic!("parse {}: {e}", path.display()))
    };
    let v1 = read("1.18.31.json");
    let v2 = read("2.0.28.json");
    let ops = |spec: &serde_json::Value| -> std::collections::HashSet<(String, String)> {
        let mut out = std::collections::HashSet::new();
        for (path, item) in spec["paths"].as_object().expect("paths") {
            for method in item.as_object().expect("item").keys() {
                if matches!(method.as_str(), "get" | "post" | "put" | "patch" | "delete") {
                    let router_path = match path.strip_suffix("/*") {
                        Some(base) => format!("{base}/{{*path}}"),
                        None => path.clone(),
                    };
                    out.insert((method.to_uppercase(), router_path));
                }
            }
        }
        out
    };
    let expected: std::collections::HashSet<(String, String)> =
        ops(&v2).difference(&ops(&v1)).cloned().collect();
    let actual: std::collections::HashSet<(String, String)> = table().into_iter().collect();
    let missing: Vec<_> = expected.difference(&actual).collect();
    let extra: Vec<_> = actual.difference(&expected).collect();
    assert!(
        missing.is_empty() && extra.is_empty(),
        "v2-only.tsv is stale (run scripts/gen-v2-gate.py)\nmissing: {missing:?}\nextra: {extra:?}"
    );
}
