//! Embedded, version-matched web UI (WEBUI-PLAN P6).
//!
//! Upstream's `serveUIEffect` is **embedded-first**: the release binary embeds
//! the whole `packages/app/dist` and serves it at `/` + `/assets/*`, proxying
//! to `app.opencode.ai` only when embedding is disabled. ocserve historically
//! had only the proxy — so it served a *newer* Cloudflare UI against the
//! frozen server (version skew; the exact bug class the user hit). This module
//! restores upstream's priority: serve the pinned 1.18.31 build first, fall
//! back to the proxy only under `OCSERVE_DISABLE_EMBEDDED_WEB_UI=1` (mirroring
//! upstream's `OPENCODE_DISABLE_EMBEDDED_WEB_UI`).
//!
//! The pinned build is a single zstd pack (`bench/webui/app/<tag>.pack.zst`,
//! produced by `scripts/extract-app.sh` through the freeze's PUBLIC HTTP
//! interface — not binary scraping). Pack format before zstd: `OCAP1\n` +
//! u32 count + (u16 pathlen, path, u64 datalen, data)*. Decompressed once at
//! boot into an in-memory map (bounded: the closure is ~10 MB raw).
//!
//! Air-gapped property: an unknown asset under the embedded arm returns the
//! upstream JSON 404 `{"error":"Not Found"}` — it never falls through to the
//! network. That is exactly upstream's embedded behavior.

use std::collections::HashMap;
use std::sync::OnceLock;

/// The pinned pack. `include_bytes!` keeps `/` working on a bare machine with
/// no network and no data dir (the "boots with defaults" invariant).
static PACK_ZST: &[u8] = include_bytes!("../../../bench/webui/app/1.18.31.pack.zst");

/// The freeze tag the pack was built from — must match `FREEZE_VERSION`.
pub const APP_TAG: &str = "1.18.31";

static FILES: OnceLock<HashMap<String, Vec<u8>>> = OnceLock::new();

/// Decompress + parse the pack once. Returns the path→bytes map (empty on a
/// malformed pack — never panics, so a corrupt artifact degrades to the proxy
/// rather than killing boot).
fn files() -> &'static HashMap<String, Vec<u8>> {
    FILES.get_or_init(|| {
        let raw = match zstd::stream::decode_all(PACK_ZST) {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!("embedded UI pack: zstd decode failed: {e:#}");
                return HashMap::new();
            }
        };
        parse_pack(&raw).unwrap_or_else(|e| {
            tracing::warn!("embedded UI pack: parse failed: {e}");
            HashMap::new()
        })
    })
}

fn parse_pack(raw: &[u8]) -> Result<HashMap<String, Vec<u8>>, String> {
    if raw.len() < 6 || &raw[..6] != b"OCAP1\n" {
        return Err("bad magic".into());
    }
    let mut i = 6usize;
    let count = read_u32(raw, &mut i)? as usize;
    let mut map = HashMap::with_capacity(count);
    for _ in 0..count {
        let plen = read_u16(raw, &mut i)? as usize;
        if i + plen > raw.len() {
            return Err("path overruns".into());
        }
        let path = String::from_utf8(raw[i..i + plen].to_vec()).map_err(|_| "bad path utf8")?;
        i += plen;
        let dlen = read_u64(raw, &mut i)? as usize;
        if i + dlen > raw.len() {
            return Err("data overruns".into());
        }
        let data = raw[i..i + dlen].to_vec();
        i += dlen;
        map.insert(path, data);
    }
    Ok(map)
}

fn read_u16(b: &[u8], i: &mut usize) -> Result<u16, String> {
    if *i + 2 > b.len() {
        return Err("eof u16".into());
    }
    let v = u16::from_le_bytes([b[*i], b[*i + 1]]);
    *i += 2;
    Ok(v)
}
fn read_u32(b: &[u8], i: &mut usize) -> Result<u32, String> {
    if *i + 4 > b.len() {
        return Err("eof u32".into());
    }
    let v = u32::from_le_bytes([b[*i], b[*i + 1], b[*i + 2], b[*i + 3]]);
    *i += 4;
    Ok(v)
}
fn read_u64(b: &[u8], i: &mut usize) -> Result<u64, String> {
    if *i + 8 > b.len() {
        return Err("eof u64".into());
    }
    let v = u64::from_le_bytes(b[*i..*i + 8].try_into().map_err(|_| "u64")?);
    *i += 8;
    Ok(v)
}

/// True when the embedded pack is present and parsed (non-empty).
pub fn available() -> bool {
    !files().is_empty()
}

/// Look up a request path in the embedded build. Mirrors upstream
/// `serveEmbeddedUIEffect`: `/` and unknown non-asset paths fall back to
/// `index.html` (SPA routing); `/assets/*` is exact (an unknown asset is a
/// 404, never `index.html`). Returns `(effective_path, bytes)` so the caller
/// uses the resolved file's MIME (an SPA-fallback `/route` is text/html).
pub fn lookup(path: &str) -> Option<(&'static str, &'static [u8])> {
    let map = files();
    let key = path.trim_start_matches('/');
    if let Some((k, data)) = map.get_key_value(key) {
        // k borrows the 'static map, so it outlives the borrow of `path`.
        return Some((k.as_str(), data));
    }
    // SPA fallback: only for non-asset paths (assets must 404, else a missing
    // font would be served HTML and the browser would choke).
    if !path.starts_with("/assets/")
        && let Some(data) = map.get("index.html")
    {
        return Some(("index.html", data));
    }
    None
}

/// MIME type by extension (small table; matches what freeze serves).
pub fn mime_for(path: &str) -> &'static str {
    let p = path.split('?').next().unwrap_or(path);
    match p.rsplit('.').next().unwrap_or("") {
        "html" => "text/html",
        "js" | "mjs" => "text/javascript",
        "css" => "text/css",
        "json" | "webmanifest" => "application/json",
        "svg" => "image/svg+xml",
        "png" => "image/png",
        "ico" => "image/x-icon",
        "ttf" => "font/ttf",
        "woff" => "font/woff",
        "woff2" => "font/woff2",
        "mp4" => "video/mp4",
        "wasm" => "application/wasm",
        "txt" => "text/plain",
        _ => "application/octet-stream",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pack_parses_and_has_entrypoints() {
        // The committed pack must parse and carry the SPA entry + its JS.
        assert!(available(), "embedded pack did not parse");
        let map = files();
        assert!(map.contains_key("index.html"), "no index.html");
        // entry JS is content-hashed; find one
        assert!(
            map.keys()
                .any(|k| k.starts_with("assets/index-") && k.ends_with(".js")),
            "no entry JS"
        );
    }

    #[test]
    fn spa_fallback_only_for_non_assets() {
        // unknown non-asset → index.html (resolved path is the html entry)
        let (resolved, idx) = lookup("/some/spa/route").expect("spa fallback");
        assert_eq!(resolved, "index.html");
        assert!(String::from_utf8_lossy(idx).contains("<"));
        // unknown asset → None (must 404, never HTML)
        assert!(lookup("/assets/does-not-exist-xyz.js").is_none());
    }

    #[test]
    fn mime_table() {
        assert_eq!(mime_for("/assets/index-abc.js"), "text/javascript");
        assert_eq!(mime_for("/assets/index-abc.css"), "text/css");
        assert_eq!(mime_for("/assets/font.woff2"), "font/woff2");
        assert_eq!(mime_for("/"), "application/octet-stream");
        assert_eq!(mime_for("/index.html"), "text/html");
    }
}
