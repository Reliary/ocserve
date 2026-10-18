# Security Policy

## Reporting a Vulnerability

If you discover a security vulnerability in ocserve, please report it
privately via GitHub's private vulnerability reporting:

**https://github.com/Reliary/ocserve/security/advisories/new**

**Do not open a public GitHub issue.**

We will acknowledge receipt within 48 hours and provide an estimated timeline
for a fix.

## Scope

- The `ocserve` binary (server, CLI, MCP-capable tool host, importer)
- The embedded, version-matched opencode web UI served at `/`
- The plugin sidecar host (`ocserve-plugin`) and MCP client (`ocserve-mcp`)
- The PTY group (`ocserve-pty`)
- Build and release infrastructure

## Out of Scope

- LLM provider API keys (user-managed; ocserve never logs them)
- Third-party dependencies (managed via `cargo-deny` and Dependabot)
- The upstream opencode project itself (see its own policy)

## Threat model / posture

- ocserve reads the user's existing `~/.config/opencode` config, auth, and
  model cache **read-only** and never writes to them.
- Default bind is loopback. Exposure to a network is an operator decision
  (e.g. a Tailscale ACL or a socat forwarder); when bound non-loopback the auth
  policy is the frozen upstream contract (`off` by default, password via
  `OCSERVE_SERVER_PASSWORD` when set). **Fail-fast (2026-10-18):** binding a
  non-loopback address with `auth_mode=off` is refused at boot — the credential
  store echoes provider API keys on GET, so an open network-reachable bind would
  expose them. Use `auth_mode="basic"` (with a mode-600 password file), or keep
  the bind loopback and expose via the tailnet forwarder (the deployed pattern).
- The credential store (`credential` table) holds provider API keys. The DB
  directory is locked to 0700 and the unit sets `UMask=0077`; SQLite's recreated
  WAL/SHM inherit owner-only access. `GET /api/credential` echoes values (upstream
  parity) — safe only because the server is loopback or auth-gated.
- Access to filesystem routes is contained to the session worktree
  (`D-FS-CONTAIN`; writers additionally resolve the deepest existing ancestor so
  a symlink cannot redirect a create out of the tree); MCP tool trust is
  TOFU-pinned with an enforce mode.

## Current known divergences

The parity ledger (`PLAN.md §17`, `bench/*/allow.txt`) names every intentional
difference from upstream. One is security-relevant and documented, not hidden:

- `D-PTY-1`: the PTY connect-token accepts a request with no `Origin` — safe on
  a loopback bind, a hazard on a non-loopback bind.

## Security Practices

- **SAST**: CodeQL runs on every push/PR (`.github/workflows/codeql-analysis.yml`)
- **Dependency scanning**: Dependabot + `cargo-deny` advisories/sources/bans
- **Secret scanning**: gitleaks on every PR diff (`.github/workflows/pr-secret-scan.yml`)
- **Supply chain**: `cargo-audit` + `cargo-deny` in CI and nightly
- **Fuzz**: `cargo-fuzz` targets for the parsers (weekly)
- **Unsafe**: `#![forbid(unsafe_code)]` on every crate that can carry it; the
  sole opt-in (`ocserve-pty` FFI, `ocserve-http` glibc `malloc_trim`) is
  documented per-site and asserted in CI
- **Least privilege**: every workflow token is scoped to the minimum

## Branch Protection

`main` is protected: pull request required, the CI status checks
(`gates`, `replay`, `capdiff`, `pair`) must pass, and direct pushes are
restricted. See `.github/workflows/`.
