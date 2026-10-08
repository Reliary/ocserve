//! Web-UI backend support (Phase 7): upstream-parity CORS + the catch-all
//! UI proxy.
//!
//! Upstream v1 semantics (probed live 2026-08/10):
//! - CORS (`server/src/cors.ts`): allowed = no Origin, `http://localhost:*`,
//!   `http://127.0.0.1:*`, `oc://renderer`, `tauri://localhost`,
//!   `http(s)://tauri.localhost`, `https://*.opencode.ai`, plus user extras
//!   (config `server.cors` + `--cors`). Disallowed origins get NO ACAO header
//!   (not a rejected request). Preflight → 204, methods
//!   GET,HEAD,PUT,PATCH,POST,DELETE, allow-headers echoes requested, max-age
//!   86400.
//! - Catch-all (`shared/ui.ts` serveUIEffect): any unmatched path serves the
//!   web app — embedded assets when present, otherwise a reverse proxy to
//!   `https://app.opencode.ai`. The hosted app talks to `location.origin`
//!   when served same-origin, so proxying makes `http://127.0.0.1:4096/`
//!   a fully working self-hosted UI. HTML proxied through gets the upstream
//!   CSP (theme-preload script hash included).
//!
//! Divergences (documented): `OCSERVE_UI=0` disables the proxy (JSON 404
//! instead) for air-gapped/CI use; `OCSERVE_UI_UPSTREAM` overrides the URL.

use axum::body::Body;
use axum::extract::State;
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use std::sync::Arc;

pub const DEFAULT_UI_UPSTREAM: &str = "https://app.opencode.ai";

/// Upstream predicate (cors.ts isAllowedCorsOrigin), verbatim rules.
pub fn is_allowed_cors_origin(origin: &str, extra: &[String]) -> bool {
    let origin = origin.trim();
    if origin.is_empty() {
        return true; // no Origin header
    }
    if origin.starts_with("http://localhost:") || origin.starts_with("http://127.0.0.1:") {
        return true;
    }
    if origin.starts_with("oc://renderer") {
        return true;
    }
    if origin == "tauri://localhost"
        || origin == "http://tauri.localhost"
        || origin == "https://tauri.localhost"
    {
        return true;
    }
    if is_opencode_ai_origin(origin) {
        return true;
    }
    extra.iter().any(|e| e == origin)
}

/// `https://([a-z0-9-]+\.)*opencode\.ai` — hand-rolled (no regex dep) with
/// exact case/character rules: the host must be `opencode.ai` or a subdomain
/// of lowercase alnum/hyphen labels.
fn is_opencode_ai_origin(origin: &str) -> bool {
    let Some(rest) = origin.strip_prefix("https://") else {
        return false;
    };
    if rest.contains('/') || rest.contains(':') {
        return false;
    }
    let labels: Vec<&str> = rest.split('.').collect();
    if labels.len() < 2 {
        return false;
    }
    if labels[labels.len() - 2] != "opencode" || labels[labels.len() - 1] != "ai" {
        return false;
    }
    labels.iter().all(|l| {
        !l.is_empty()
            && l.chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
    })
}

/// Hand-rolled CORS middleware with exact freeze parity (probed live):
/// preflight → 204 with ACAO + echoed ACAH + ACM 4 methods... actually the
/// freeze method list is GET, HEAD, PUT, PATCH, POST, DELETE, ACAH echoes the
/// requested list, max-age 86400, `Vary: Access-Control-Request-Headers,
/// Origin`. Simple requests get ACAO only. Disallowed origins pass through
/// WITHOUT headers (upstream never rejects the request itself).
pub async fn cors_gate(
    State(st): State<Arc<crate::AppState>>,
    req: axum::extract::Request,
    next: axum::middleware::Next,
) -> Response {
    use axum::http::Method;
    let origin = req
        .headers()
        .get(header::ORIGIN)
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    let allowed = origin
        .as_deref()
        .map(|o| is_allowed_cors_origin(o, &st.cors_extra))
        .unwrap_or(false);

    let is_preflight = req.method() == Method::OPTIONS
        && req.headers().contains_key("access-control-request-method");
    if is_preflight && allowed {
        let Some(origin) = origin else {
            return next.run(req).await;
        };
        let mut resp = StatusCode::NO_CONTENT.into_response();
        let h = resp.headers_mut();
        h.insert(
            "access-control-allow-origin",
            HeaderValue::from_str(&origin).unwrap_or(HeaderValue::from_static("*")),
        );
        h.insert(
            "vary",
            HeaderValue::from_static("Access-Control-Request-Headers, Origin"),
        );
        h.insert(
            "access-control-allow-methods",
            HeaderValue::from_static("GET, HEAD, PUT, PATCH, POST, DELETE"),
        );
        // echo the requested headers (freeze behavior; empty when absent)
        if let Some(reqh) = req.headers().get("access-control-request-headers").cloned() {
            h.insert("access-control-allow-headers", reqh);
        } else {
            h.insert("access-control-allow-headers", HeaderValue::from_static(""));
        }
        h.insert("access-control-max-age", HeaderValue::from_static("86400"));
        return resp;
    }

    let mut resp = next.run(req).await;
    if allowed
        && let Some(origin) = origin
        && let Ok(v) = HeaderValue::from_str(&origin)
    {
        resp.headers_mut().insert("access-control-allow-origin", v);
    }
    resp
}

/// The UI proxy decision, frozen at boot (env is read once — SRE fail-fast
/// discipline; changing it = restart).
pub struct UiProxy {
    pub enabled: bool,
    pub upstream: String,
    client: reqwest::Client,
}

impl UiProxy {
    pub fn from_env() -> Self {
        let enabled = std::env::var("OCSERVE_UI")
            .map(|v| v != "0" && !v.eq_ignore_ascii_case("off"))
            .unwrap_or(true);
        let upstream = std::env::var("OCSERVE_UI_UPSTREAM")
            .ok()
            .filter(|u| !u.is_empty())
            .unwrap_or_else(|| DEFAULT_UI_UPSTREAM.to_string());
        Self::new(enabled, upstream)
    }

    pub fn new(enabled: bool, upstream: String) -> Self {
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(30))
            .build()
            .unwrap_or_default();
        Self {
            enabled,
            upstream,
            client,
        }
    }
}

/// CSP mirror of upstream shared/ui.ts csp(): identical directives; when the
/// HTML carries the theme-preload inline script, its sha256 is added.
fn csp(hash: &str) -> String {
    format!(
        "default-src 'self'; script-src 'self' 'wasm-unsafe-eval'{}; style-src 'self' 'unsafe-inline'; img-src 'self' data: https: blob:; font-src 'self' data:; media-src 'self' data:; connect-src * data: blob:",
        if hash.is_empty() {
            String::new()
        } else {
            format!(" 'sha256-{hash}'")
        }
    )
}

/// Extract the theme-preload script body for the CSP hash (upstream
/// themePreloadHash): `<script ... id="oc-theme-preload-script">…</script>`.
fn theme_preload_script(body: &str) -> Option<&str> {
    let needle = "id=\"oc-theme-preload-script\"";
    let alt = "id='oc-theme-preload-script'";
    let start = body.find(needle).or_else(|| body.find(alt))?;
    // find the matching `>` that closes the opening tag, then the closing tag
    let open_end = body[start..].find('>')? + start;
    let close = body[open_end..].find("</script>")? + open_end;
    Some(&body[open_end + 1..close])
}

fn csp_for_html(body: &str) -> String {
    use base64::Engine as _;
    use sha2::Digest as _;
    match theme_preload_script(body) {
        Some(script) => {
            let digest = sha2::Sha256::digest(script.as_bytes());
            csp(&base64::engine::general_purpose::STANDARD.encode(digest))
        }
        None => csp(""),
    }
}

/// Axum fallback: reverse-proxy unmatched paths to the UI upstream
/// (upstream v1 serveUIEffect). Disabled → JSON 404 `{"error":"Not Found"}`
/// (upstream's own notFound() shape when embedded assets are missing).
pub async fn ui_fallback(
    State(st): State<Arc<crate::AppState>>,
    req: axum::extract::Request,
) -> Response {
    let ui = &st.ui;
    if !ui.enabled {
        return (
            StatusCode::NOT_FOUND,
            axum::Json(serde_json::json!({"error": "Not Found"})),
        )
            .into_response();
    }
    let path = req
        .uri()
        .path_and_query()
        .map(|pq| pq.as_str())
        .unwrap_or("/");
    let url = format!("{}{}", ui.upstream.trim_end_matches('/'), path);

    let method = reqwest::Method::from_bytes(req.method().as_str().as_bytes())
        .unwrap_or(reqwest::Method::GET);
    let mut builder = ui.client.request(method, &url);
    // Forward headers except Host (we dial the upstream) and hop-by-hop.
    builder = builder.headers(forward_headers(req.headers()));
    // Body (bounded by axum's own limits; forwarded as a stream).
    let body = axum::body::to_bytes(req.into_body(), 64 * 1024 * 1024)
        .await
        .unwrap_or_default();
    if !body.is_empty() {
        builder = builder.body(body);
    }

    let resp = match builder.send().await {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!("ui proxy: {url} failed: {e:#}");
            return (
                StatusCode::BAD_GATEWAY,
                axum::Json(serde_json::json!({"error": "UI upstream unreachable"})),
            )
                .into_response();
        }
    };

    let status = StatusCode::from_u16(resp.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
    let content_type = resp
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    let is_html = content_type
        .as_deref()
        .map(|c| c.contains("text/html"))
        .unwrap_or(false);
    let bytes = resp.bytes().await.unwrap_or_default();

    let mut out = Response::builder().status(status);
    if let Some(ct) = &content_type {
        out = out.header(header::CONTENT_TYPE, ct);
    }
    let csp_value = if is_html {
        csp_for_html(&String::from_utf8_lossy(&bytes))
    } else {
        csp("")
    };
    out = out.header("content-security-policy", csp_value);
    out.body(Body::from(bytes))
        .unwrap_or_else(|_| StatusCode::BAD_GATEWAY.into_response())
}

fn forward_headers(src: &HeaderMap) -> HeaderMap {
    let mut out = HeaderMap::new();
    for (name, value) in src {
        // host = our host; hop-by-hop + length/encoding handled by reqwest
        if matches!(
            name.as_str(),
            "host" | "content-length" | "transfer-encoding" | "connection" | "accept-encoding"
        ) {
            continue;
        }
        out.insert(name.clone(), value.clone());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cors_matches_upstream_rules() {
        let none: Vec<String> = vec![];
        // allowed
        assert!(is_allowed_cors_origin("http://localhost:5173", &none));
        assert!(is_allowed_cors_origin("http://127.0.0.1:4096", &none));
        assert!(is_allowed_cors_origin("oc://renderer", &none));
        assert!(is_allowed_cors_origin("tauri://localhost", &none));
        assert!(is_allowed_cors_origin("http://tauri.localhost", &none));
        assert!(is_allowed_cors_origin("https://tauri.localhost", &none));
        assert!(is_allowed_cors_origin("https://opencode.ai", &none));
        assert!(is_allowed_cors_origin("https://app.opencode.ai", &none));
        assert!(is_allowed_cors_origin("https://a-b.c.opencode.ai", &none));
        // disallowed
        assert!(!is_allowed_cors_origin("https://evil.com", &none));
        assert!(!is_allowed_cors_origin("http://opencode.ai", &none)); // http, not https
        assert!(!is_allowed_cors_origin(
            "https://opencode.ai.evil.com",
            &none
        ));
        assert!(!is_allowed_cors_origin("https://OPEnc0de.ai", &none)); // case-sensitive
        assert!(!is_allowed_cors_origin("https://opencode.ai:8080", &none));
        // extras
        let extra = vec!["https://my.box".to_string()];
        assert!(is_allowed_cors_origin("https://my.box", &extra));
        assert!(!is_allowed_cors_origin("https://my.box", &none));
    }

    #[test]
    fn theme_preload_hash_extraction() {
        let html = r#"<script id="oc-theme-preload-script">window.x=1</script>"#;
        assert_eq!(theme_preload_script(html), Some("window.x=1"));
        let html2 = r#"<script defer id='oc-theme-preload-script'>y</script>"#;
        assert_eq!(theme_preload_script(html2), Some("y"));
        assert_eq!(theme_preload_script("<html></html>"), None);
        // csp includes the hash when present, and is the base policy otherwise
        let with = csp_for_html(html);
        assert!(with.contains("sha256-"), "{with}");
        assert!(with.contains("'sha256-"), "{with}");
        let without = csp_for_html("<html></html>");
        assert!(!without.contains("sha256"));
    }
}
