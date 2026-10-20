# Contributing

## Build

```bash
cargo build --release        # binary at target/release/ocserve
```

## Test

```bash
cargo test --workspace       # unit + integration (offline)
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --check
./scripts/check-guards.sh    # static bug-class rules
./scripts/check-matrix.sh    # TRACEABILITY requirement rows exist
```

Differential gates (need the vendored freeze binary; see `bench/pair/`):

```bash
./scripts/replay-check.sh
./scripts/pair-check.sh
./scripts/capdiff-check.sh --selftest --full
```

## PR Workflow

1. Branch from `main`: `feat/<area>-<slug>`, `fix/<issue>-<slug>`,
   `test/<slug>`, `docs/<slug>`.
2. Keep commits small and conventional (`feat:`, `fix:`, `docs:`, `test:`).
   A `crates/**/*.rs` change must stage a `TRACEABILITY.md` row (pre-commit).
3. Run the gates above. All must pass locally before opening the PR.
4. Open a PR against `main`. CI runs exactly the gates you ran plus the
   differential suite in `capdiff`/`pair`.

## Release

Releases are tag-triggered; nothing is built by hand.

1. Bump `[workspace.package] version` in `Cargo.toml` (all `workspace.dependencies`
   entries in the same file follow it) and commit as `release: vX.Y.Z`.
2. Push the commit, then tag: `git tag -a vX.Y.Z -m "..." && git push origin vX.Y.Z`.
3. `.github/workflows/release.yml` builds linux x86_64 + aarch64, packages
   tarballs with `SHA256SUMS`, and attaches them to the GitHub Release.
   macOS/Windows are best-effort (the PTY shim is Linux/glibc).
4. Deploy a host with `./scripts/install.sh --release` (fetches and verifies the
   artifact; no Rust toolchain needed).

crates.io is separate and manual (`publish.yml`, `workflow_dispatch`, requires
`CARGO_REGISTRY_TOKEN`). It is not on the release path; `check-crate-payload.py`
(rule 38) fails the build if any crate would exceed the registry's upload limit.

## Docs are the spec

The core documents below are normative and disagreements are fixed as doc
commits (precedence: `TESTING.md` → `PLAN.md` → domain spec):

| Doc | Holds |
|---|---|
| `PLAN.md` | scope, milestones, parity ledger §17 |
| `STORAGE.md` | pragmas, schema, blob/DB crash protocol |
| `SRE.md` | boot checks, metrics, config knobs, runbook |
| `MEMORY.md` | memory budget and allocation rules |
| `TESTING.md` | test levels, anti-theater rules, bug-class table |
| `SNAPSHOT.md` | file snapshots, revert/unrevert/session-diff, `/vcs/apply` |
| `TRACEABILITY.md` | requirement → test → status matrix |

## Code style

- Edition 2024; formatting pinned by `rustfmt.toml` (`cargo fmt --check`).
- No `unwrap`/`expect`/`panic` in server paths (`ocserve-*` runtime crates).
- No `unbounded_channel` in runtime code; every channel/cache/pool is bounded
  by construction.
- No new `unsafe`: the crate-root policy is `#![forbid(unsafe_code)]` except
  the documented opt-ins (`ocserve-pty`, `ocserve-http`'s `malloc_trim`).
- Storage goes through the shared `open()` routine; pragmas never inlined.
- No personal names or absolute home paths anywhere (use `~`/`$HOME`); the
  pre-commit hook enforces this.

## Anti-theater

A bug fix lands with a test that fails without the fix (a negative control),
and a guard that would have caught the class. See `TESTING.md §1`. A test that
passes with its guard removed is theatre and will be rejected.
