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

/// Reference pin for the v2 line (see `V2-PLAN.md` D3).
pub const V2_TAG: &str = "2.0.28";

/// The regenerated 2.0.28 OpenAPI document. Unlike the v1 asset this is NOT a
/// verbatim upstream file: the checked-in `packages/protocol/openapi.json` is
/// stale (2026-10-13: 136 ops vendored vs 140 declared), so this asset is the
/// output of upstream's own pipeline (`OpenApi.fromApi(ClientApi)` + the
/// content-addressed stabilize step) run against the PUBLISHED npm tarballs
/// `@opencode/protocol@2.0.28` + `@opencode/schema@2.0.28`. Regenerate with
/// `scripts/extract-spec.sh 2.0.28 --regen` (proven byte-reproducible
/// 2026-10-16: 118 paths / 142 ops / 303 schemas).
pub const SPEC_V2_JSON: &str = include_str!("../assets/openapi/2.0.28.json");
