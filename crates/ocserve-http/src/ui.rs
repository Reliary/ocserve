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

/// Lazy, byte-accounted LRU for proxied static assets (WEBUI-PLAN W3).
///
/// Memory honesty (the user's question, 2026-10-08): this holds **zero bytes
/// until the first asset is fetched through the proxy**, and zero forever if
/// the web UI is never used (`OCSERVE_UI=0` disables the proxy entirely). The
/// 32 MiB is a cap, not a reservation. Entries are whole assets (identity +
/// negotiated encoding is served from the same stored body; compression for
/// assets is done once when they enter the cache). Byte-bounded FIFO so a
/// single huge asset cannot pin unbounded memory.
pub const ASSET_CACHE_CAP: usize = 32 * 1024 * 1024;
/// Largest single asset we will cache (a pathological upstream artifact
/// larger than this streams through uncached rather than displacing the
/// whole cache).
pub const ASSET_ENTRY_CAP: usize = 8 * 1024 * 1024;

#[derive(Clone)]
pub struct AssetEntry {
    pub body: bytes::Bytes, // identity bytes
    pub br: bytes::Bytes,
    pub gzip: bytes::Bytes,
    pub content_type: String,
    pub etag: String,
}

struct CacheState {
    map: std::collections::HashMap<String, std::sync::Arc<AssetEntry>>,
    order: std::collections::VecDeque<String>,
    bytes: usize,
}

/// The UI proxy decision + lazy asset cache, frozen at boot (env is read
/// once — SRE fail-fast discipline; changing it = restart).
pub struct UiProxy {
    pub enabled: bool,
    pub upstream: String,
    client: reqwest::Client,
    cache: parking_lot::Mutex<CacheState>,
    /// OPT-IN prewarm (default off; OCSERVE_UI_PREWARM=1). When on, the entry
    /// HTML's referenced assets are fetched once at boot so the first browser
    /// load is warm. Off by default per WEBUI-PLAN W3.
    pub prewarm: bool,
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
        let prewarm = std::env::var("OCSERVE_UI_PREWARM")
            .map(|v| v == "1")
            .unwrap_or(false);
        let mut p = Self::new(enabled, upstream);
        p.prewarm = prewarm;
        p
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
            cache: parking_lot::Mutex::new(CacheState {
                map: std::collections::HashMap::new(),
                order: std::collections::VecDeque::new(),
                bytes: 0,
            }),
            prewarm: false,
        }
    }

    /// Cache lookup. None = miss. Metrics are emitted by the caller (which
    /// knows whether it is a hit or miss + the byte count).
    fn cache_get(&self, key: &str) -> Option<std::sync::Arc<AssetEntry>> {
        let mut c = self.cache.lock();
        let e = c.map.get(key).cloned()?;
        // refresh recency (FIFO with LRU-ish touch: move to back)
        if let Some(pos) = c.order.iter().position(|k| k == key) {
            let k = c.order.remove(pos).expect("position");
            c.order.push_back(k);
        }
        Some(e)
    }

    /// Insert into the byte-bounded FIFO; evict from the front until it
    /// fits. Oversized single entries are refused (streamed uncached).
    fn cache_put(&self, key: String, entry: std::sync::Arc<AssetEntry>) {
        let sz = entry.body.len() + entry.br.len() + entry.gzip.len();
        if sz > ASSET_ENTRY_CAP {
            return;
        }
        let mut c = self.cache.lock();
        if c.map.contains_key(&key) {
            return;
        }
        while c.bytes + sz > ASSET_CACHE_CAP {
            let Some(front) = c.order.pop_front() else {
                break;
            };
            if let Some(old) = c.map.remove(&front) {
                c.bytes = c
                    .bytes
                    .saturating_sub(old.body.len() + old.br.len() + old.gzip.len());
            }
        }
        c.bytes += sz;
        c.map.insert(key.clone(), entry);
        c.order.push_back(key);
        ocserve_metrics::gauge("ocserve_webui_asset_cache_bytes", c.bytes as i64);
    }

    /// True when the request path is a cacheable static asset (content-hashed
    /// name under /assets/). Only these are cached; HTML and API-ish proxied
    /// paths stream through.
    fn is_cacheable_asset(path: &str) -> bool {
        path.starts_with("/assets/")
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
    // cache key + validators captured before the body is consumed
    let req_path = req.uri().path().to_string();
    let req_headers_ae = req
        .headers()
        .get(header::ACCEPT_ENCODING)
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    let req_headers_inm = req
        .headers()
        .get(header::IF_NONE_MATCH)
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);

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
    let upstream_cache_control = resp
        .headers()
        .get(reqwest::header::CACHE_CONTROL)
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    let is_asset = UiProxy::is_cacheable_asset(&req_path);

    // W3: cacheable assets — serve from the lazy cache on hit (with ETag/304
    // and negotiation), populate on miss.
    if is_asset && status == StatusCode::OK {
        let cache_key = req_path.to_string();
        let ae = req_headers_ae.as_deref();
        let inm = req_headers_inm.as_deref();
        if let Some(entry) = ui.cache_get(&cache_key) {
            ocserve_metrics::counter("ocserve_webui_asset_cache_hits_total", 1);
            return asset_response(&entry, ae, inm, true);
        }
        ocserve_metrics::counter("ocserve_webui_asset_cache_misses_total", 1);
        let bytes = resp.bytes().await.unwrap_or_default();
        let ct = content_type
            .clone()
            .unwrap_or_else(|| "application/octet-stream".into());
        // Compression is CPU-bound (brotli-q5 on a 2.8 MB bundle); run it off
        // the async workers (AGENTS §2 / F1 convoy class) — never block a
        // tokio worker on compression.
        let entry = tokio::task::spawn_blocking(move || {
            let etag = crate::compress::etag_for(&bytes);
            std::sync::Arc::new(AssetEntry {
                br: bytes::Bytes::from(crate::compress::compress(
                    &bytes,
                    crate::compress::Encoding::Brotli,
                )),
                gzip: bytes::Bytes::from(crate::compress::compress(
                    &bytes,
                    crate::compress::Encoding::Gzip,
                )),
                body: bytes,
                content_type: ct,
                etag,
            })
        })
        .await
        .unwrap_or_else(|_| {
            std::sync::Arc::new(AssetEntry {
                body: bytes::Bytes::new(),
                br: bytes::Bytes::new(),
                gzip: bytes::Bytes::new(),
                content_type: "application/octet-stream".into(),
                etag: "\"\"".into(),
            })
        });
        // only cache non-empty results (a failed compression fallback is empty)
        if !entry.body.is_empty() {
            ui.cache_put(cache_key, entry.clone());
        }
        return asset_response(&entry, ae, inm, false);
    }

    // Non-asset (HTML, API-ish, any error status): stream through, no cache.
    // CSP is injected on HTML (theme-preload hash); other content gets the
    // base policy — exactly the pre-W3 behavior except the body is no longer
    // fully buffered when it is large/streaming.
    if is_html {
        let bytes = resp.bytes().await.unwrap_or_default();
        let csp_value = csp_for_html(&String::from_utf8_lossy(&bytes));
        let mut out = Response::builder().status(status);
        if let Some(ct) = &content_type {
            out = out.header(header::CONTENT_TYPE, ct);
        }
        // pass upstream cache-control through if present (CN sends no-store)
        if let Some(cc) = upstream_cache_control {
            out = out.header(header::CACHE_CONTROL, cc);
        }
        out = out.header("content-security-policy", csp_value);
        return out
            .body(Body::from(bytes))
            .unwrap_or_else(|_| StatusCode::BAD_GATEWAY.into_response());
    }

    // stream everything else
    let mut out = Response::builder().status(status);
    if let Some(ct) = &content_type {
        out = out.header(header::CONTENT_TYPE, ct);
    }
    if let Some(cc) = upstream_cache_control {
        out = out.header(header::CACHE_CONTROL, cc);
    }
    out = out.header("content-security-policy", csp(""));
    out.body(Body::from_stream(resp.bytes_stream()))
        .unwrap_or_else(|_| StatusCode::BAD_GATEWAY.into_response())
}

/// Serve a cached asset with negotiation + validator; identity clients get
/// byte-identical identity bytes.
fn asset_response(
    entry: &AssetEntry,
    accept_encoding: Option<&str>,
    if_none_match: Option<&str>,
    cached: bool,
) -> Response {
    use axum::http::{StatusCode, header};
    let _ = cached;
    if crate::compress::if_none_match_matches(if_none_match, &entry.etag) {
        return Response::builder()
            .status(StatusCode::NOT_MODIFIED)
            .header(header::ETAG, &entry.etag)
            .header(header::CACHE_CONTROL, "public, max-age=31536000, immutable")
            .header(header::VARY, "Accept-Encoding")
            .body(Body::empty())
            .expect("static 304");
    }
    let enc = crate::compress::negotiate(accept_encoding);
    let mut b = Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, &entry.content_type)
        .header(header::ETAG, &entry.etag)
        // content-hashed filename → safe to cache forever (RFC 9111 §5.2.2)
        .header(header::CACHE_CONTROL, "public, max-age=31536000, immutable")
        .header(header::VARY, "Accept-Encoding");
    let body = match enc {
        Some(crate::compress::Encoding::Brotli) => {
            b = b.header(header::CONTENT_ENCODING, "br");
            entry.br.clone()
        }
        Some(crate::compress::Encoding::Gzip) => {
            b = b.header(header::CONTENT_ENCODING, "gzip");
            entry.gzip.clone()
        }
        None => entry.body.clone(),
    };
    b.body(Body::from(body)).expect("static asset")
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

/// W3 opt-in prewarm: fetch the entry HTML, parse referenced /assets/*,
/// fetch each once so the lazy cache is warm. Bounded (the app references a
/// handful); best-effort — any network error returns without touching state.
pub async fn prewarm_assets(st: &Arc<crate::AppState>) -> anyhow::Result<()> {
    let ui = &st.ui;
    if !ui.enabled {
        return Ok(());
    }
    let base = ui.upstream.trim_end_matches('/');
    let html = ui
        .client
        .get(format!("{base}/"))
        .send()
        .await?
        .text()
        .await?;
    let mut assets: Vec<String> = Vec::new();
    for part in html.split('"').filter(|s| s.starts_with("/assets/")) {
        let a = part.split(['"', '?']).next().unwrap_or("");
        if a.starts_with("/assets/") && !assets.iter().any(|x| x == a) {
            assets.push(a.to_string());
        }
    }
    for a in &assets {
        // Route through our own handler path by constructing the URL and
        // doing the same fetch+compress the cold path does.
        let resp = ui.client.get(format!("{base}{a}")).send().await;
        let Ok(resp) = resp else { continue };
        if resp.status().as_u16() != 200 {
            continue;
        }
        let ct = resp
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("application/octet-stream")
            .to_string();
        let bytes = resp.bytes().await.unwrap_or_default();
        if bytes.is_empty() || bytes.len() > ASSET_ENTRY_CAP {
            continue;
        }
        let ct_c = ct.clone();
        let entry = tokio::task::spawn_blocking(move || {
            let etag = crate::compress::etag_for(&bytes);
            std::sync::Arc::new(AssetEntry {
                br: bytes::Bytes::from(crate::compress::compress(
                    &bytes,
                    crate::compress::Encoding::Brotli,
                )),
                gzip: bytes::Bytes::from(crate::compress::compress(
                    &bytes,
                    crate::compress::Encoding::Gzip,
                )),
                body: bytes,
                content_type: ct_c,
                etag,
            })
        })
        .await?;
        ui.cache_put(a.clone(), entry);
        tracing::info!("ui prewarm: cached {a}");
    }
    Ok(())
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
