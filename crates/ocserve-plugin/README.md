# ocserve-plugin

opencode plugin host for `ocserve`: a Node/Bun sidecar speaking NDJSON
JSON-RPC, the full upstream hook-name table (chat.message, transforms,
tool.execute, event bus, compaction), heap/RSS guards with respawn
replay, and the rolldown-based plugin normalizer.

Plugin compatibility targets opencode v1 (freeze 1.18.31); v2 plugin
entrypoints are out of scope by policy (`PLAN.md` §8).
