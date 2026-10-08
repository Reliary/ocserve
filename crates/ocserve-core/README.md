# ocserve-core

Session domain for `ocserve`: inbox admission, the agent loop (rounds,
finalize, compaction triggers), the durable event bus, permission and
question gates, and the loop-guard detectors.

Consumed by `ocserve-cli`; not intended as a standalone API yet. The
wire-visible behavior it produces is specified in the repository's
`PLAN.md`/`COMPACTION.md` and locked by the differential replay corpus.
