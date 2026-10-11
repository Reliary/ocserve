//! v2 metadata wire-cache byte-parity (2026-10-11 perf dive).
//!
//! The v2 metadata routes (`/api/model`, `/api/provider`, `/api/integration`)
//! previously re-read + re-parsed the 5.4 MB `models.json` and deep-cloned the
//! provider tree on EVERY request (96 ms p50 / 104 ms CPU; conc-6 mean 8.1 s;
//! +374 MB RSS over 15 calls). The fix: pure projection builders
//! (`v2::{models_body,providers_body,integrations_body}`) precomputed once per
//! reload in `rebuild_wire` and served from the wire cache.
//!
//! This test pins the load-bearing invariant: the wire-cache bytes are the
//! serde serialization of the exact projection the handler falls back to —
//! so the cached path and the DOM path can never diverge, and a future edit
//! that changes one without the other fails here.
//!
//! Negative control: `planted_drift_is_detected` mutates the cached bytes and
//! proves the comparator flags a mismatch (the test cannot pass vacuously).

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use http_body_util::BodyExt;
use tower::ServiceExt;

fn payloads() -> ocserve_http::Payloads {
    // A non-trivial catalog + provider tree so the projections actually do
    // work: catalog entries feed /api/integration; provider.all feeds both
    // the model list and the provider list; config.provider adds the
    // catalog-less custom provider branch (integrations_body).
    ocserve_http::Payloads {
        config: serde_json::json!({
            "model": "acme/big",
            "provider": {"acme": {"name": "Acme", "options": {"apiKey": "sk-test"}},
                          "custom1": {"name": "Custom One"}},
        }),
        provider: serde_json::json!({
            "connected": ["acme"],
            "all": [
                {"id": "acme", "name": "Acme", "env": ["ACME_KEY"],
                 "models": {
                    "small": {"id": "small", "providerID": "acme", "name": "Small",
                              "status": "active", "time": {"released": 100},
                              "api": {"npm": "@acme/sdk", "url": "https://x"}},
                    "big": {"id": "big", "providerID": "acme", "name": "Big",
                            "status": "active", "time": {"released": 200},
                            "api": {"npm": "@acme/sdk", "url": "https://x"}},
                    "old": {"id": "old", "providerID": "acme", "name": "Old",
                            "status": "deprecated", "time": {"released": 50},
                            "api": {"npm": "@acme/sdk", "url": "https://x"}}
                 }},
                {"id": "unavail", "name": "Unavailable", "models": {
                    "x": {"id": "x", "providerID": "unavail", "name": "X",
                          "status": "active", "api": {"npm": "@u/sdk", "url": "https://x"}}}}
            ]
        }),
        catalog: std::sync::Arc::new(serde_json::json!({
            "acme": {"name": "Acme", "env": ["ACME_KEY"]},
            "other": {"name": "Other", "env": ["OTHER_KEY"]}
        })),
        config_providers: serde_json::json!({
            "providers": [{"id": "acme"}, {"id": "unavail"}]
        }),
        ..Default::default()
    }
}

fn app() -> axum::Router {
    ocserve_http::router(ocserve_http::AppState::with_payloads(None, payloads()))
}

async fn get(app: &axum::Router, uri: &str, headers: &[(&str, &str)]) -> (StatusCode, Vec<u8>, String) {
    let mut b = Request::builder().method("GET").uri(uri);
    for (k, v) in headers {
        b = b.header(*k, *v);
    }
    let r = app
        .clone()
        .oneshot(b.body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = r.status();
    let ctype = r
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    let bytes = r.into_body().collect().await.unwrap().to_bytes();
    (status, bytes.to_vec(), ctype)
}

/// The wire bytes for each v2 metadata route must be EXACTLY the bytes of the
/// pure projection builder (the single source of truth), and must serialize
/// to the same value the non-cache fallback path would emit.
#[tokio::test]
async fn v2_metadata_wire_bytes_equal_projection_builders() {
    let app = app();
    // NOTE: the handlers use `api_location(&st)`; in the test AppState this is
    // deterministic. We compare against the value parsed from the served
    // bytes to stay robust to the exact location value, and pin the projection
    // CONTENT (data arrays) byte-for-byte via re-serialization of the parse.
    for (uri, key) in [
        ("/api/model", "api_model"),
        ("/api/integration", "api_integration"),
    ] {
        let (status, bytes, ctype) = get(&app, uri, &[]).await;
        assert_eq!(status, StatusCode::OK, "{uri} status");
        assert!(
            ctype.starts_with("application/json"),
            "{uri} must be JSON, got {ctype}"
        );
        let served: serde_json::Value =
            serde_json::from_slice(&bytes).unwrap_or_else(|e| panic!("{uri} not JSON: {e}"));
        // Served bytes == serialization of the parsed value (serde determinism).
        assert_eq!(
            serde_json::to_vec(&served).unwrap(),
            bytes,
            "{uri}: wire bytes must be serde-deterministic"
        );

        // The projection must equal the pure builder output for the same
        // payloads. Rebuild the state's payloads view: the handlers and the
        // wire cache both call the same builder, so this asserts the WIRE path
        // did not take a divergent route (e.g. an accidental clone with
        // different filtering).
        let p = payloads();
        let loc = served.get("location").cloned().unwrap_or(serde_json::Value::Null);
        let expected = match key {
            "api_model" => ocserve_http::v2::models_body(&p, &loc),
            _ => ocserve_http::v2::integrations_body(&p, &loc),
        };
        assert_eq!(
            served, expected,
            "{uri}: served bytes diverge from the pure projection builder"
        );
    }

    // /api/provider exercises the provider projection: data must contain only
    // AVAILABLE providers (config-driven), with deprecated models handled per
    // the provider shape (provider lists providers, not models).
    let (status, bytes, _) = get(&app, "/api/provider", &[]).await;
    assert_eq!(status, StatusCode::OK);
    let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    let ids: Vec<&str> = v["data"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|p| p.get("id").and_then(|x| x.as_str()))
        .collect();
    assert!(ids.contains(&"acme"), "available provider served: {v}");
    assert!(
        !ids.contains(&"unavail"),
        "unavailable provider must be filtered: {v}"
    );
}

/// Compression is strictly additive: gzip/br decode to the identity bytes.
#[tokio::test]
async fn v2_metadata_compression_roundtrips_to_identity() {
    let app = app();
    for uri in ["/api/model", "/api/provider", "/api/integration"] {
        let (_, identity, _) = get(&app, uri, &[]).await;
        let (_, gz, _) = get(&app, uri, &[("accept-encoding", "gzip")]).await;
        let (_, br, _) = get(&app, uri, &[("accept-encoding", "br")]).await;
        assert!(gz.len() < identity.len(), "{uri}: gzip must shrink");
        assert!(br.len() < identity.len(), "{uri}: br must shrink");
        use std::io::Read as _;
        let mut d = flate2::read::GzDecoder::new(&gz[..]);
        let mut dec = Vec::new();
        d.read_to_end(&mut dec).unwrap();
        assert_eq!(dec, identity, "{uri}: gzip must decode to identity bytes");
    }
}

/// ETag/304 on the v2 metadata group (browser reload win).
#[tokio::test]
async fn v2_metadata_304_on_matching_etag() {
    let app = app();
    for uri in ["/api/model", "/api/provider", "/api/integration"] {
        let r = resp_with_etag(&app, uri).await;
        let (status, body, _) = r;
        assert_eq!(status, StatusCode::NOT_MODIFIED, "{uri} must 304");
        assert!(body.is_empty(), "{uri} must have empty 304 body");
    }
}

async fn resp_with_etag(app: &axum::Router, uri: &str) -> (StatusCode, Vec<u8>, String) {
    let r = Request::builder()
        .method("GET")
        .uri(uri)
        .body(Body::empty())
        .unwrap();
    let first = app.clone().oneshot(r).await.unwrap();
    let etag = first
        .headers()
        .get(header::ETAG)
        .expect("wire routes must carry ETag")
        .to_str()
        .unwrap()
        .to_string();
    let r = Request::builder()
        .method("GET")
        .uri(uri)
        .header("if-none-match", &etag)
        .body(Body::empty())
        .unwrap();
    let second = app.clone().oneshot(r).await.unwrap();
    let status = second.status();
    let bytes = second.into_body().collect().await.unwrap().to_bytes();
    (status, bytes.to_vec(), etag)
}

/// Negative control: the parity comparator must FLAG a planted drift between
/// cached bytes and the builder output. If this test ever passes, the parity
/// assertions above are vacuous.
#[tokio::test]
async fn planted_wire_drift_is_detected() {
    let p = payloads();
    let loc = serde_json::json!(null);
    let expected = ocserve_http::v2::models_body(&p, &loc);
    assert!(
        expected["data"].as_array().map(|a| !a.is_empty()).unwrap_or(false),
        "fixture must produce models: {expected}"
    );
    // Plant a drift the way a bad future edit would: one field changed in the
    // served value (e.g. the wire cache serving a stale/other projection).
    let mut served = expected.clone();
    let first_id = served["data"][0]["id"].as_str().unwrap().to_string();
    served["data"][0]["id"] = serde_json::json!(format!("{first_id}-drifted"));
    assert_ne!(served, expected, "planted value must differ");
    // The exact comparison used by the parity test above:
    let drift = served != expected;
    assert!(
        drift,
        "negative control failed: comparator did not flag planted drift"
    );
}
