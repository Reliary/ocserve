# ocserve-http

The axum router implementing the opencode server wire contract (freeze
1.18.31): sessions, messages with cursor paging, SSE event streams with a
byte-bounded ring, config/provider/agent/command derived routes, MCP
control, permissions, questions, the question gate, and the error
envelope.

Wire shapes are byte-locked by `testdata/golden` differential replay. See
`PLAN.md` §3 (contract freeze) and §16–17 (route audits and divergences)
in the repository root.
