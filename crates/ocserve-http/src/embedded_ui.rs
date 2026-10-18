//! Embedded, version-matched web UI (WEBUI-PLAN P6 + V2-PARITY Phase E).
//!
//! Upstream's `serveUIEffect` is **embedded-first**: the release binary embeds
//! the whole app build and serves it at `/` + asset prefixes, proxying to
//! `app.opencode.ai` only when embedding is disabled. ocserve historically
//! had only the proxy — so it served a *newer* Cloudflare UI against the
//! frozen server (version skew; the exact bug class the user hit). This module
//! restores upstream's priority per pack tag, selected by `OCSERVE_UI_PACK`
//! (`1.18.31` default; `2.0.28` alongside `OCSERVE_V2=1`), falling back to
//! the proxy only under `OCSERVE_DISABLE_EMBEDDED_WEB_UI=1` (mirroring
//! upstream's `OPENCODE_DISABLE_EMBEDDED_WEB_UI`).
//!
//! Each pinned build is a single zstd pack (`assets/webui/<tag>.pack.zst`,
//! produced by `scripts/extract-app.sh` through each binary's PUBLIC HTTP
//! interface — not binary scraping). Pack format before zstd: `OCAP1\n` +
//! u32 count + (u16 pathlen, path, u64 datalen, data)*. Decompressed once at
//! boot into an in-memory map (bounded: each closure is ~7 MB raw).
//!
//! Air-gapped property: an unknown asset under the embedded arm returns the
//! upstream JSON 404 `{"error":"Not Found"}` — it never falls through to the
//! network. That is exactly upstream's embedded behavior.

use std::sync::OnceLock;

/// The pinned v1 pack. `include_bytes!` keeps `/` working on a bare machine
/// with no network and no data dir (the "boots with defaults" invariant).
///
/// Lives INSIDE this crate (`assets/webui/<tag>.pack.zst`) so it is packaged
/// and published with the crate; embedding from `bench/` (outside the crate
/// root) is not in the `.crate` tarball, so `cargo install ocserve` fails to
/// compile it (the 0.3.0 defect; guard rule 30). Produced by
/// `scripts/extract-app.sh` through the freeze's public HTTP interface.
static PACK_ZST: &[u8] = include_bytes!("../assets/webui/1.18.31.pack.zst");

/// The pinned v2 pack (Phase E): extracted from the real
/// `@opencode/cli-linux-x64@2.0.28` binary's own HTTP surface (173 files,
/// 6.4 MB raw → 3.2 MB zstd). Same pipeline, second tag.
static PACK_V2_ZST: &[u8] = include_bytes!("../assets/webui/2.0.28.pack.zst");

/// The freeze tag the v1 pack was built from — must match `FREEZE_VERSION`.
pub const APP_TAG: &str = "1.18.31";

/// The v2 tag the second pack was built from — must match `SPEC_V2_TAG`.
pub const APP_TAG_V2: &str = "2.0.28";

/// Which pack serves `/`. `OCSERVE_UI_PACK=2.0.28` selects the v2 shell
/// (natural pair with `OCSERVE_V2=1`); anything else serves v1. Explicit
/// flag, never sniffed: both packs claim `/`, so first-load auto-detect is
/// impossible.
pub fn active_tag() -> &'static str {
    tag_for(std::env::var("OCSERVE_UI_PACK").ok().as_deref())
}

/// Pure selection (testable without touching process-global env — env
/// mutation in parallel tests is a flake class, TESTING §1.6).
fn tag_for(value: Option<&str>) -> &'static str {
    match value {
        Some("2.0.28") => APP_TAG_V2,
        _ => APP_TAG,
    }
}

static FILES: OnceLock<Pack> = OnceLock::new();
static FILES_V2: OnceLock<Pack> = OnceLock::new();

/// A parsed OCAP2 pack: an index of (path → frame location) plus the raw blob
/// for lazy per-file decompression. The closure can be large (the full v2
/// closure is ~87 MB raw, mostly lazily-loaded office-preview WASM), so we do
/// NOT materialize every file — an `index.html`-only map is kept resident and
/// each asset decompresses on demand into a small bounded LRU. Decompression
/// failure for one file is a miss, never a panic (fail-soft, like the old
/// whole-pack decode).
pub struct Pack {
    /// path (no leading slash) → (frame_offset, raw_len, comp_len)
    index: std::collections::HashMap<String, (usize, usize, usize)>,
    blob: &'static [u8],
    cache: std::sync::Mutex<Cache>,
}

/// Bounded LRU for decompressed files (MEMORY.md: caches declare a bound).
/// 16 MiB covers the entry bundle + fonts + the largest single WASM; a big
/// lazy chunk evicts the previous one rather than growing without limit.
const CACHE_BYTES: usize = 16 * 1024 * 1024;

struct Cache {
    map: std::collections::HashMap<String, bytes::Bytes>,
    order: std::collections::VecDeque<String>,
    bytes: usize,
}

impl Pack {
    fn get(&self, key: &str) -> Option<bytes::Bytes> {
        if let Ok(c) = self.cache.lock()
            && let Some(v) = c.map.get(key)
        {
            return Some(v.clone());
        }
        let (off, raw_len, comp_len) = *self.index.get(key)?;
        let frame = self.blob.get(off..off + comp_len)?;
        let data = zstd::stream::decode_all(frame).ok()?;
        debug_assert_eq!(data.len(), raw_len);
        let arc = bytes::Bytes::from(data);
        if let Ok(mut c) = self.cache.lock() {
            c.bytes += arc.len();
            c.map.insert(key.to_string(), arc.clone());
            c.order.push_back(key.to_string());
            while c.bytes > CACHE_BYTES && c.order.len() > 1 {
                if let Some(old) = c.order.pop_front()
                    && let Some(ev) = c.map.remove(&old)
                {
                    c.bytes -= ev.len();
                }
            }
        }
        Some(arc)
    }

    fn contains(&self, key: &str) -> bool {
        self.index.contains_key(key)
    }
}

/// Parse an OCAP2 pack's index (no per-file decompress). Falls back to the
/// legacy OCAP1 eager parse so an unmigrated committed pack still loads.
fn parse_pack(pack: &'static [u8]) -> Pack {
    if pack.len() >= 6 && &pack[..6] == b"OCAP2\n" {
        return parse_ocap2(pack);
    }
    // Legacy OCAP1: eagerly decode every file (previous behavior).
    parse_ocap1(pack)
}

fn parse_ocap2(blob: &'static [u8]) -> Pack {
    let mut i = 6usize;
    let ok = (|| -> Result<Pack, String> {
        let count = read_u32(blob, &mut i)? as usize;
        let mut idx = std::collections::HashMap::with_capacity(count);
        let mut specs = Vec::with_capacity(count);
        for _ in 0..count {
            let plen = read_u16(blob, &mut i)? as usize;
            if i + plen > blob.len() {
                return Err("path overruns".into());
            }
            let path =
                String::from_utf8(blob[i..i + plen].to_vec()).map_err(|_| "bad path utf8")?;
            i += plen;
            let off = read_u64(blob, &mut i)? as usize;
            let raw_len = read_u32(blob, &mut i)? as usize;
            let comp_len = read_u32(blob, &mut i)? as usize;
            specs.push((path, off, raw_len, comp_len));
        }
        let data_base = i;
        for (path, off, raw_len, comp_len) in specs {
            let abs = data_base + off;
            if abs + comp_len > blob.len() {
                return Err("frame overruns".into());
            }
            idx.insert(path, (abs, raw_len, comp_len));
        }
        Ok(Pack {
            index: idx,
            blob,
            cache: std::sync::Mutex::new(Cache {
                map: std::collections::HashMap::new(),
                order: std::collections::VecDeque::new(),
                bytes: 0,
            }),
        })
    })();
    ok.unwrap_or_else(|e| {
        tracing::warn!("embedded UI pack: OCAP2 parse failed: {e}");
        empty_pack(blob)
    })
}

fn empty_pack(blob: &'static [u8]) -> Pack {
    Pack {
        index: std::collections::HashMap::new(),
        blob,
        cache: std::sync::Mutex::new(Cache {
            map: std::collections::HashMap::new(),
            order: std::collections::VecDeque::new(),
            bytes: 0,
        }),
    }
}

/// Legacy OCAP1 (whole-pack zstd of an eager file table). Kept so a committed
/// pack made before the OCAP2 migration still boots.
fn parse_ocap1(pack: &'static [u8]) -> Pack {
    let raw = match zstd::stream::decode_all(pack) {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!("embedded UI pack: zstd decode failed: {e:#}");
            return empty_pack(pack);
        }
    };
    let mut idx = std::collections::HashMap::new();
    let mut cache = Cache {
        map: std::collections::HashMap::new(),
        order: std::collections::VecDeque::new(),
        bytes: 0,
    };
    // store every file eagerly in the cache (bounded by the pack's own size)
    if raw.len() >= 6 && &raw[..6] == b"OCAP1\n" {
        let mut i = 6usize;
        let count = read_u32(&raw, &mut i).unwrap_or(0) as usize;
        for _ in 0..count {
            let Ok(plen) = read_u16(&raw, &mut i) else {
                break;
            };
            let plen = plen as usize;
            if i + plen > raw.len() {
                break;
            }
            let Ok(path) = String::from_utf8(raw[i..i + plen].to_vec()) else {
                break;
            };
            i += plen;
            let Ok(dlen) = read_u64(&raw, &mut i) else {
                break;
            };
            let dlen = dlen as usize;
            if i + dlen > raw.len() {
                break;
            }
            let data = raw[i..i + dlen].to_vec();
            i += dlen;
            idx.insert(path.clone(), (0, 0, 0));
            cache.bytes += data.len();
            cache.map.insert(path.clone(), bytes::Bytes::from(data));
            cache.order.push_back(path);
        }
    }
    // leak the eager map into a static-compatible blob via Box::leak so the
    // lifetime matches the 'static signature (OCAP1 path is transitional).
    let leaked: &'static [u8] = Box::leak(raw.into_boxed_slice());
    Pack {
        index: idx,
        blob: leaked,
        cache: std::sync::Mutex::new(cache),
    }
}

fn files() -> &'static Pack {
    FILES.get_or_init(|| parse_pack(PACK_ZST))
}

fn files_v2() -> &'static Pack {
    FILES_V2.get_or_init(|| parse_pack(PACK_V2_ZST))
}

/// The active pack (flag-selected; see `active_tag`).
fn active_files() -> &'static Pack {
    if active_tag() == APP_TAG_V2 {
        files_v2()
    } else {
        files()
    }
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

/// True when the ACTIVE embedded pack is present and parsed (non-empty).
pub fn available() -> bool {
    !active_files().index.is_empty()
}

/// Look up a request path in the ACTIVE embedded build. Mirrors upstream
/// `serveEmbeddedUIEffect`: `/` and unknown non-asset paths fall back to
/// `index.html` (SPA routing); asset prefixes (`/assets/`, `/_assets/`,
/// `/icons/`) are exact (an unknown asset is a 404, never `index.html`).
/// Returns `(effective_path, bytes)`: the file is decompressed on demand (the
/// returned buffer is refcounted; the pack cache bounds total resident bytes).
pub fn lookup(path: &str) -> Option<(String, bytes::Bytes)> {
    let pack = active_files();
    let key = path.trim_start_matches('/');
    if pack.contains(key) {
        return pack.get(key).map(|data| (key.to_string(), data));
    }
    // SPA fallback: only for non-asset paths (assets must 404, else a missing
    // font would be served HTML and the browser would choke).
    if !path.starts_with("/assets/")
        && !path.starts_with("/_assets/")
        && !path.starts_with("/icons/")
    {
        return pack
            .get("index.html")
            .map(|data| ("index.html".to_string(), data));
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
        // The committed packs must parse and carry the SPA entry + its JS.
        for pack in [files(), files_v2()] {
            assert!(!pack.index.is_empty(), "embedded pack did not parse");
            assert!(pack.contains("index.html"), "no index.html");
        }
        // v1 entry JS is content-hashed under assets/
        assert!(
            files()
                .index
                .keys()
                .any(|k| k.starts_with("assets/index-") && k.ends_with(".js")),
            "no v1 entry JS"
        );
        // v2 entry under _assets/ (proves the extract regex caught the prefix)
        assert!(
            files_v2()
                .index
                .keys()
                .any(|k| k.starts_with("_assets/") && k.ends_with(".js")),
            "no v2 _assets entry"
        );
        // lazy decompression works: the entry bundle decompresses to valid bytes
        let entry = files_v2()
            .get("_assets/index-Bn6xbm4C.js")
            .expect("v2 entry present");
        assert!(entry.len() > 1000, "entry bundle too small");
    }

    #[test]
    fn active_tag_selects_pack() {
        // Pure selection: default (None/unknown) is v1; 2.0.28 selects v2.
        assert_eq!(tag_for(None), APP_TAG);
        assert_eq!(tag_for(Some("9.9.9")), APP_TAG);
        assert_eq!(tag_for(Some("2.0.28")), APP_TAG_V2);
        // CONTROL: default that selects v2 → first assert fails, red.
    }

    #[test]
    fn spa_fallback_only_for_non_assets() {
        // unknown non-asset → index.html (resolved path is the html entry)
        let (resolved, idx) = lookup("/some/spa/route").expect("spa fallback");
        assert_eq!(resolved, "index.html");
        assert!(String::from_utf8_lossy(&idx).contains("<"));
        // unknown asset → None (must 404, never HTML) — all three prefixes
        assert!(lookup("/assets/does-not-exist-xyz.js").is_none());
        assert!(lookup("/_assets/does-not-exist-xyz.js").is_none());
        assert!(lookup("/icons/does-not-exist-xyz.png").is_none());
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
