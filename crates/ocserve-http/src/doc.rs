//! `GET /doc` — the OpenAPI contract (freeze parity).
//!
//! opencode 1.18.31 serves its complete OpenAPI 3.1.0 document at `/doc`
//! (162 paths / 188 operations / 472 schemas, covering v1 + `/api/*` v2 +
//! `/sync/*` + `/tui/*` in one tagged document). We vendor that exact document
//! and serve it byte-for-byte so ocserve is self-describing and the
//! spec-driven coverage guard / response validation have a stable oracle.
//!
//! The document is embedded at compile time so `/doc` works on a bare machine
//! with no config or network (consistent with the "boots with defaults"
//! invariant). Regenerate with `scripts/extract-spec.sh <tag>`.

/// The frozen 1.18.31 document, byte-identical to the upstream tag's
/// `packages/sdk/openapi.json` and to freeze's live `GET /doc` (verified
/// 2026-10-08).
pub const SPEC_JSON: &str = include_str!("../../../bench/openapi/1.18.31.json");
