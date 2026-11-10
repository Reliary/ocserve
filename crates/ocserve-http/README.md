# ocserve-http

The axum router implementing the opencode server wire contract (frozen at
1.19.5): sessions, messages with cursor paging, SSE event streams with a
byte-bounded ring, config/provider/agent/command derived routes, MCP
control, permissions, questions, the question gate, and the error
envelope.

Wire shapes are byte-locked by `testdata/golden` differential replay. See
`PLAN.md` §3 (contract freeze) and §18 (divergences)
in the repository root.
