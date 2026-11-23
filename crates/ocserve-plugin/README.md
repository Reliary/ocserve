# ocserve-plugin

opencode plugin host for `ocserve`: a Node/Bun sidecar speaking NDJSON
JSON-RPC, the full upstream hook-name table (chat.message, transforms,
tool.execute, event bus, compaction), heap/RSS guards with respawn
replay, and the rolldown-based plugin normalizer.

The host prefers Bun when it is installed. The Node fallback covers the
documented `bun:sqlite` shim, but cannot load plugins that use Bun-only APIs
such as `bun:ffi` (`opencode-pty` is one example). For plugins that load a
native `bun-pty` library, the host resolves the matching library from the
original package tree before importing the normalized entry and sets
`BUN_PTY_LIB` per plugin; a value the operator set at launch always wins.
Musl hosts try the `_musl` library names first.

Plugin compatibility targets opencode v1 (frozen at 1.19.5); v2 plugin
entrypoints are out of scope by policy (`PLAN.md` §8).
