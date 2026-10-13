//! The frozen OpenAPI contract, embedded at compile time.
//!
//! opencode 1.18.31 serves its complete OpenAPI 3.1.0 document at `/doc`
//! (162 paths / 188 operations / 472 schemas, covering v1 + `/api/*` v2 +
//! `/sync/*` + `/tui/*` in one tagged document). We vendor that exact document
//! and serve it byte-for-byte so ocserve is self-describing and the
//! spec-driven coverage guard / response validation have a stable oracle.
//!
//! The document lives INSIDE this crate (`assets/openapi/<tag>.json`) so it is
//! packaged and published with the crate — an artifact embedded from a path
//! outside the crate root is not in the `.crate` tarball, so `cargo install`
//! fails to compile it (the 0.3.0 defect; guard rule 30). Regenerate with
//! `scripts/extract-spec.sh <tag>`.
//!
//! `GET /doc` works on a bare machine with no config or network (the "boots
//! with defaults" invariant) because the bytes are embedded.

/// The freeze tag the vendored contract was extracted from. Must match
/// `FREEZE_VERSION` / `/global/health`.
pub const TAG: &str = "1.18.31";

/// The frozen 1.18.31 OpenAPI document, byte-identical to the upstream tag's
/// `packages/sdk/openapi.json` and to freeze's live `GET /doc` (verified
/// 2026-10-08; re-verified per release).
pub const SPEC_JSON: &str = include_str!("../assets/openapi/1.18.31.json");
