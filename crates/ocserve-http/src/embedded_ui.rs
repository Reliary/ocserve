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
//! Each pinned build is an OCAP2 blob: an index plus per-file zstd frames,
//! produced by `scripts/extract-app.sh` through each binary's PUBLIC HTTP
//! interface (not binary scraping). Because crates.io caps a single `.crate`
//! at 10 MiB and the closures are 10.2 MB / 25.6 MB, the frame region is
//! split at frame boundaries into `ocserve-ui-*` crates (`scripts/shard-pack.py`);
//! the index stays here. The loader maps an absolute frame offset to the part
//! that contains it, so the runtime format, the 16 MiB cache and the binary
//! size are unchanged. Nothing is dropped to fit the registry.
//!
//! Air-gapped property: an unknown asset under the embedded arm returns the
//! upstream JSON 404 `{"error":"Not Found"}` — it never falls through to the
//! network. That is exactly upstream's embedded behavior.

use std::sync::OnceLock;

/// Index (paths + frame offsets) for the v1 pack. `include_bytes!` keeps `/`
/// working on a bare machine with no network and no data dir (the "boots with
/// defaults" invariant). The frame bytes live in `ocserve-ui-11831-*` because
/// crates.io caps one crate at 10 MiB; the index is small (42 KB).
static PACK_INDEX: &[u8] = include_bytes!("../assets/webui/1.18.31.index.bin");
static PACK_V2_INDEX: &[u8] = include_bytes!("../assets/webui/2.0.28.index.bin");

/// Frame bytes for v1, in pack order.
static PACK_V1_PARTS: &[&[u8]] = &[ocserve_ui_11831_a::PART, ocserve_ui_11831_b::PART];

/// Frame bytes for v2 (the 2.0.28 closure), in pack order.
static PACK_V2_PARTS: &[&[u8]] = &[
    ocserve_ui_2028_a::PART,
    ocserve_ui_2028_b::PART,
    ocserve_ui_2028_c::PART,
    ocserve_ui_2028_d::PART,
];

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
    blob: ShardedBlob,
    cache: std::sync::Mutex<Cache>,
}

/// The concatenated frame region, backed by several `include_bytes!` parts.
/// Offsets in the index are absolute within this logical blob; a range may
/// span two parts (each frame is cut at a boundary, but the accessor handles
/// spanning so a future split cannot silently corrupt a file).
struct ShardedBlob {
    parts: &'static [&'static [u8]],
    len: usize,
}

impl ShardedBlob {
    fn new(parts: &'static [&'static [u8]]) -> Self {
        Self {
            len: parts.iter().map(|p| p.len()).sum(),
            parts,
        }
    }

    fn slice(&self, off: usize, len: usize) -> Option<bytes::Bytes> {
        let end = off.checked_add(len)?;
        if end > self.len {
            return None;
        }
        let mut at = 0usize;
        let mut out: Option<Vec<u8>> = None;
        for part in self.parts {
            let part_end = at + part.len();
            if off < part_end && end > at {
                let lo = off.saturating_sub(at);
                let hi = (end - at).min(part.len());
                match &mut out {
                    None if lo == 0 && hi == part.len() && std::ptr::eq(*part, self.parts[0]) => {
                        // fast path below
                    }
                    None => out = Some(part[lo..hi].to_vec()),
                    Some(v) => v.extend_from_slice(&part[lo..hi]),
                }
            }
            at = part_end;
            if at >= end {
                break;
            }
        }
        match out {
            Some(v) => Some(bytes::Bytes::from(v)),
            None => {
                // Single part, zero-copy slice.
                let mut at = 0usize;
                for part in self.parts {
                    let part_end = at + part.len();
                    if off >= at && end <= part_end {
                        return Some(bytes::Bytes::copy_from_slice(&part[off - at..end - at]));
                    }
                    at = part_end;
                }
                None
            }
        }
    }
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
        let frame = self.blob.slice(off, comp_len)?;
        let data = zstd::stream::decode_all(&frame[..]).ok()?;
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

/// Parse an OCAP2 INDEX (the header + entries, no frames) and attach the
/// sharded frame region. The index is generated by `scripts/shard-pack.py`
/// alongside the parts; it repeats the OCAP2 header so the parser is the same.
fn parse_pack(index: &'static [u8], parts: &'static [&'static [u8]]) -> Pack {
    let mut i = 6usize;
    let ok = (|| -> Result<Pack, String> {
        if index.len() < 6 || &index[..6] != b"OCAP2\n" {
            return Err("not an OCAP2 index".into());
        }
        let count = read_u32(index, &mut i)? as usize;
        let mut idx = std::collections::HashMap::with_capacity(count);
        for _ in 0..count {
            let plen = read_u16(index, &mut i)? as usize;
            if i + plen > index.len() {
                return Err("path overruns".into());
            }
            let path =
                String::from_utf8(index[i..i + plen].to_vec()).map_err(|_| "bad path utf8")?;
            i += plen;
            let off = read_u64(index, &mut i)? as usize;
            let raw_len = read_u32(index, &mut i)? as usize;
            let comp_len = read_u32(index, &mut i)? as usize;
            idx.insert(path, (off, raw_len, comp_len));
        }
        Ok(Pack {
            index: idx,
            blob: ShardedBlob::new(parts),
            cache: std::sync::Mutex::new(Cache {
                map: std::collections::HashMap::new(),
                order: std::collections::VecDeque::new(),
                bytes: 0,
            }),
        })
    })();
    ok.unwrap_or_else(|e| {
        tracing::warn!("embedded UI index: parse failed: {e}");
        empty_pack(parts)
    })
}

fn empty_pack(parts: &'static [&'static [u8]]) -> Pack {
    Pack {
        index: std::collections::HashMap::new(),
        blob: ShardedBlob::new(parts),
        cache: std::sync::Mutex::new(Cache {
            map: std::collections::HashMap::new(),
            order: std::collections::VecDeque::new(),
            bytes: 0,
        }),
    }
}

fn files() -> &'static Pack {
    FILES.get_or_init(|| parse_pack(PACK_INDEX, PACK_V1_PARTS))
}

fn files_v2() -> &'static Pack {
    FILES_V2.get_or_init(|| parse_pack(PACK_V2_INDEX, PACK_V2_PARTS))
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

    /// The frame region is split across crates; an offset near a part
    /// boundary must still resolve, and a spanning range must be assembled.
    /// Without this the first file after a split would silently 404.
    #[test]
    fn sharded_blob_reads_across_part_boundaries() {
        let a: &'static [u8] = b"AAAA";
        let b: &'static [u8] = b"BBBB";
        static PARTS: &[&[u8]] = &[b"AAAA", b"BBBB"];
        let blob = ShardedBlob::new(PARTS);
        assert_eq!(&blob.slice(0, 4).unwrap()[..], b"AAAA");
        assert_eq!(&blob.slice(4, 4).unwrap()[..], b"BBBB");
        assert_eq!(&blob.slice(2, 4).unwrap()[..], b"AABB");
        assert_eq!(&blob.slice(3, 2).unwrap()[..], b"AB");
        assert!(blob.slice(7, 2).is_none(), "overrun must fail, never panic");
        let _ = (a, b);
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
