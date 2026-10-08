//! Content negotiation for the web surfaces (WEBUI-PLAN.md W1/W2).
//!
//! Design law: the wire contract is the freeze's — a client that sends
//! neither `Accept-Encoding` nor `If-None-Match` must receive **byte-identical**
//! responses to what ocserve served before this module existed (WEBUI-PLAN
//! target 3, asserted by replay/pair/k6 and by tests here). Compression and
//! validators are strictly additive, gated on the client asking.
//!
//! - `negotiate` picks an encoding from `Accept-Encoding` by q-value.
//! - `compress` produces gzip (flate2, fast level) or brotli (q5). Brotli
//!   wins on bytes for JSON/JS/CSS and is precomputed off the request path
//!   for the F5 wire routes.
//! - `etag_for` is a strong validator over the *identity* body, so the same
//!   ETag tags both the identity and compressed representations (RFC 9110
//!   §8.8.3 — a strong ETag must not change with encoding; the encoding is
//!   conveyed by Content-Encoding + Vary, so the representation is still
//!   distinct). 304 is answered before any body work.

use axum::http::header;
use axum::response::Response;

/// Encodings we can produce, best-bytes first.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Encoding {
    Brotli,
    Gzip,
}

impl Encoding {
    fn token(self) -> &'static str {
        match self {
            Encoding::Brotli => "br",
            Encoding::Gzip => "gzip",
        }
    }
}

/// Pick the client's preferred supported encoding from `Accept-Encoding`.
/// Honours q-values; `identity`/`*;q=0` suppress compression. Returns None
/// when the client did not offer anything (→ identity, the pre-existing
/// behavior).
pub fn negotiate(accept_encoding: Option<&str>) -> Option<Encoding> {
    let ae = accept_encoding?;
    // q-value parser: "gzip;q=0.5, br;q=1.0, *;q=0"
    let mut best: Option<(Encoding, f32)> = None;
    let mut identity_q: f32 = 1.0;
    let mut star_q: Option<f32> = None;
    for part in ae.split(',') {
        let mut it = part.trim().split(';');
        let token = it.next().unwrap_or("").trim().to_ascii_lowercase();
        let mut q = 1.0f32;
        for p in it {
            let p = p.trim();
            if let Some(v) = p.strip_prefix("q=") {
                q = v.trim().parse().unwrap_or(1.0);
            }
        }
        match token.as_str() {
            "gzip" => consider(&mut best, Encoding::Gzip, q),
            "br" => consider(&mut best, Encoding::Brotli, q),
            "identity" => identity_q = q,
            "*" => star_q = Some(q),
            _ => {}
        }
    }
    // If a wildcard allows an encoding we support and none was explicit,
    // prefer brotli (best bytes) when the wildcard is at least as preferred
    // as identity (RFC 9110 §12.5.3: `*` opts into any coding).
    if best.is_none()
        && let Some(sq) = star_q
        && sq >= identity_q
    {
        best = Some((Encoding::Brotli, sq));
    }
    best.filter(|(_, q)| *q > 0.0).map(|(e, _)| e)
}

fn consider(best: &mut Option<(Encoding, f32)>, enc: Encoding, q: f32) {
    if q <= 0.0 {
        return;
    }
    match best {
        Some((_, bq)) if *bq >= q => {}
        _ => *best = Some((enc, q)),
    }
}

/// Compress `data`. gzip uses the fast level (request-path friendly);
/// brotli uses quality 5 (best bytes/second tradeoff — WEBUI-PLAN measured
/// br-q11 at 20 s for 6.2 MB, rejected; q5 at 0.18 s). Precomputed callers
/// use this on the write-epoch path, not per request.
pub fn compress(data: &[u8], enc: Encoding) -> Vec<u8> {
    match enc {
        Encoding::Gzip => {
            use flate2::write::GzEncoder;
            use std::io::Write as _;
            let mut e = GzEncoder::new(Vec::new(), flate2::Compression::fast());
            // write_all to a Vec cannot fail.
            if e.write_all(data).is_err() {
                return data.to_vec();
            }
            e.finish().unwrap_or_else(|_| data.to_vec())
        }
        Encoding::Brotli => {
            use std::io::Write as _;
            let mut out = Vec::with_capacity(data.len() / 4 + 64);
            {
                let mut w = brotli::CompressorWriter::new(&mut out, 4096, 5, 22);
                if w.write_all(data).is_err() {
                    return data.to_vec();
                }
            }
            out
        }
    }
}

/// Strong ETag over the identity body: `"<hex-sha256[..16]>"` quoted per
/// RFC 9110. Stable across encodings (see module docs).
pub fn etag_for(body: &[u8]) -> String {
    use sha2::{Digest as _, Sha256};
    let d = Sha256::digest(body);
    format!("\"{}\"", hex::encode(&d[..16]))
}

/// Whether an `If-None-Match` header matches this ETag (weak/strong
/// comparison per RFC 9110 §13.1.2 — compare opaque tags, `W/` ignored on
/// the request side is not needed because we never emit weak tags).
pub fn if_none_match_matches(header_value: Option<&str>, etag: &str) -> bool {
    let Some(hv) = header_value else { return false };
    let hv = hv.trim();
    if hv == "*" {
        return true;
    }
    hv.split(',').any(|t| t.trim() == etag)
}

/// Whether a route's content type is compressible (SSE is NOT: byte-golden
/// stream, and compressing it would break framing/latency). Images/fonts
/// already-compressed are skipped.
pub fn is_compressible(content_type: &str) -> bool {
    let ct = content_type.to_ascii_lowercase();
    if ct.starts_with("text/event-stream") {
        return false; // never touch SSE
    }
    (ct.starts_with("application/json")
        || ct.starts_with("text/")
        || ct.contains("javascript")
        || ct.contains("svg"))
        && !ct.starts_with("image/")
}

/// Apply negotiation + validators to an identity body, returning a full
/// response. `identity_etag` is the caller-computed ETag (over identity
/// bytes) or None to skip validator handling.
pub fn encode_response(
    status: axum::http::StatusCode,
    content_type: &str,
    identity: bytes::Bytes,
    identity_etag: Option<&str>,
    accept_encoding: Option<&str>,
    if_none_match: Option<&str>,
    extra: &[(&str, &str)],
) -> Response {
    // 304 short-circuit: only when we have a validator and it matches.
    if let (Some(etag), Some(inm)) = (identity_etag, if_none_match)
        && if_none_match_matches(Some(inm), etag)
    {
        let mut b = Response::builder()
            .status(axum::http::StatusCode::NOT_MODIFIED)
            .header(header::ETAG, etag);
        for (k, v) in extra {
            b = b.header(*k, *v);
        }
        return b.body(axum::body::Body::empty()).expect("static 304");
    }

    let compressible = is_compressible(content_type);
    let chosen = if compressible {
        negotiate(accept_encoding)
    } else {
        None
    };

    let mut builder = Response::builder().status(status);
    for (k, v) in extra {
        builder = builder.header(*k, *v);
    }
    builder = builder.header(header::CONTENT_TYPE, content_type);
    if let Some(etag) = identity_etag {
        builder = builder.header(header::ETAG, etag);
    }
    // Vary is required whenever the response *could* differ by encoding, so
    // caches key correctly (RFC 9110 §12.5.3).
    if compressible {
        builder = builder.header(header::VARY, "Accept-Encoding");
    }

    match chosen {
        Some(enc) => {
            let compressed = compress(&identity, enc);
            builder
                .header(header::CONTENT_ENCODING, enc.token())
                .body(axum::body::Body::from(compressed))
                .expect("static response")
        }
        None => builder
            .body(axum::body::Body::from(identity))
            .expect("static response"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn negotiate_q_values_and_identity() {
        assert_eq!(negotiate(None), None);
        assert_eq!(negotiate(Some("")), None);
        assert_eq!(negotiate(Some("gzip")), Some(Encoding::Gzip));
        assert_eq!(negotiate(Some("br")), Some(Encoding::Brotli));
        // br preferred on tie (listed first, equal q → first wins)
        assert_eq!(negotiate(Some("br, gzip")), Some(Encoding::Brotli));
        // explicit q wins
        assert_eq!(
            negotiate(Some("br;q=0.5, gzip;q=1.0")),
            Some(Encoding::Gzip)
        );
        // identity;q=1 with br;q=0.5 → still br (client accepted it)
        assert_eq!(
            negotiate(Some("br;q=0.5, identity;q=1.0")),
            Some(Encoding::Brotli)
        );
        // all zero → no compression
        assert_eq!(negotiate(Some("gzip;q=0, br;q=0")), None);
        // wildcard
        assert_eq!(negotiate(Some("*")), Some(Encoding::Brotli));
        // unknown only
        assert_eq!(negotiate(Some("deflate, zstd")), None);
    }

    #[test]
    fn compress_roundtrips_shapes() {
        let data = b"hello hello hello world world world".repeat(50);
        let gz = compress(&data, Encoding::Gzip);
        let br = compress(&data, Encoding::Brotli);
        assert!(gz.len() < data.len());
        assert!(br.len() < data.len());
        // gzip magic + brotli decodes to identity
        assert_eq!(&gz[..2], &[0x1f, 0x8b]);
        // decode gzip
        use std::io::Read as _;
        let mut d = flate2::read::GzDecoder::new(&gz[..]);
        let mut out = Vec::new();
        d.read_to_end(&mut out).unwrap();
        assert_eq!(out, data);
    }

    #[test]
    fn etag_stable_and_inm() {
        let a = etag_for(b"abc");
        assert_eq!(a, etag_for(b"abc"));
        assert_ne!(a, etag_for(b"abd"));
        assert!(a.starts_with('"') && a.ends_with('"'));
        assert!(if_none_match_matches(Some(&a), &a));
        assert!(if_none_match_matches(Some("*"), &a));
        assert!(if_none_match_matches(Some(&format!("junk, {a}")), &a));
        assert!(!if_none_match_matches(Some("\"other\""), &a));
        assert!(!if_none_match_matches(None, &a));
    }

    #[test]
    fn sse_never_compressible() {
        assert!(!is_compressible("text/event-stream"));
        assert!(is_compressible("application/json"));
        assert!(is_compressible("text/html;charset=UTF-8"));
        assert!(is_compressible("text/javascript"));
        assert!(!is_compressible("image/png"));
    }

    #[test]
    fn encode_response_identity_is_byte_identical() {
        // The load-bearing invariant: no Accept-Encoding, no If-None-Match →
        // identity bytes, no Content-Encoding, no Vary on non-compressible.
        let body = bytes::Bytes::from_static(b"{\"a\":1}");
        let resp = encode_response(
            axum::http::StatusCode::OK,
            "application/json",
            body.clone(),
            None,
            None,
            None,
            &[],
        );
        assert!(resp.headers().get(header::CONTENT_ENCODING).is_none());
        // Vary present (compressible) even without negotiation — correct.
        assert_eq!(resp.headers().get(header::VARY).unwrap(), "Accept-Encoding");
    }

    #[test]
    fn encode_response_compresses_when_asked() {
        let body = bytes::Bytes::from_static(b"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa");
        let resp = encode_response(
            axum::http::StatusCode::OK,
            "application/json",
            body,
            Some("\"etag\""),
            Some("br"),
            None,
            &[],
        );
        assert_eq!(resp.headers().get(header::CONTENT_ENCODING).unwrap(), "br");
        assert_eq!(resp.headers().get(header::ETAG).unwrap(), "\"etag\"");
    }

    #[test]
    fn encode_response_304_short_circuit() {
        let body = bytes::Bytes::from_static(b"{\"a\":1}");
        let resp = encode_response(
            axum::http::StatusCode::OK,
            "application/json",
            body,
            Some("\"abc\""),
            None,
            Some("\"abc\""),
            &[],
        );
        assert_eq!(resp.status(), axum::http::StatusCode::NOT_MODIFIED);
    }
}
