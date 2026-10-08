# Parity harness — upstream opencode vs ocserve, identical workloads

Runs **opencode 1.18.31** (the freeze binary) and **ocserve** in Docker
containers against the same seeded workloads and reports resource usage and
latency side by side. Local tool first (CI can reuse the compose later).

## Quick start

```bash
cd bench/parity
./run.sh                    # preflight → warmup (discarded) →3 measured pairs → report
# report: .runs/report.md      raw: .runs/<round>/<arm>/{result.json,sampler.csv}
```

First run builds images (ocserve compiles in-container, ~5 min cold, cached
after) and stages `vendor/opencode` from the brew freeze binary.

Knobs (env): `ROUNDS=3`, `MAX_LOAD1=1.5` (quiet-host gate), `S0_IDLE_S=45`,
`S1_SESSIONS=6`, `S1_PROMPTS=4`, `S2_ITERS=30`, `S3_SUBS=4`, `S3_WRITES=4`,
`S4_SECONDS=120`, `STUB_TOK_PER_SEC=120`, `BOOT_BUDGET_S=120`.

Interrupted runs resume: rounds with a `result.json` are skipped.

## How it works

- **One shared config** (`config/opencode.json` + `auth.json` + `model.json`)
  is mounted into both arms: a stub OpenAI-compatible provider, no plugins,
  no MCP, dummy key. Both arms also get the same deterministic state default.
- **The stub** (`stub/stub.py`) derives completions from the **last user
  message only** (sha-seeded wording and length) with paced token streaming.
  Different system prompts / tool schemas across arms cannot change the
  output — seeded histories must be **byte-identical** (the report's seed
  parity table asserts this per pair; a ✗ invalidates the pair).
- **Interleaved rounds**: warmup u/r (discarded), then u,r,u,r,u,r — each
  round gets fresh containers and fresh data volumes, one arm at a time
  (never simultaneous → no contention), so host drift hits both arms.
- **Quiet-host gate**: `run.sh` refuses to start when loadavg1 > `MAX_LOAD1`
  (your live `:4901`/`:4912` services keep running untouched; gate value and
  actual load are recorded in every report).

## Scenarios

| # | What | Metrics |
|---|------|---------|
| S0 | compose → `/global/health`, then idle window | boot_s, idle current/anon |
| S1 | seed `S1_SESSIONS`×`S1_PROMPTS` sync prompts via stub | prompt latency p50/p95, wall, **seed digests** |
| S2 | shared read routes ×N (session list, message limit+cursor page, file, config, agent, command) | per-route total p50/p95 + TTFB |
| S3 | `S3_SUBS` SSE subscribers on `/global/event` + burst writes | write→first-event lag p50/p95, write→`session.idle` lag, events/s |
| S4 | post-load idle | anon slope MB/h (least squares) |

Resources come from **cgroup v2 direct reads** (memory.current, memory.peak,
memory.stat `anon`, cpu.stat) resolved via `docker inspect`; `docker stats`
is the recorded fallback. `resources.source` says which was used — never
silently mixed. `anon` is the true footprint (file-backed cache is counted
in `current` but is reclaimable).

## S5: connection storm (`STORM=1 ./run.sh`)

Models the multi-connection OOM shape (several oc-remote clients + TUI):
`S5_LIVE=4` readers that drain, `S5_SLOW=2` reading one line/5s,
`S5_STALLED=2` that complete the handshake and never read again (TCP
backpressure parks the server's writer — the backgrounded-phone case).
Phases:60s subscribers-only → sustained prompt writes (big stub frames via
`STUB_FORCE_WORDS=20000`, `STUB_TOK_PER_SEC=6000`; ring budget forced to
4MB via `OCSERVE_EVENT_RING_MB=4` so eviction/lag occur at reachable event
volumes) →60s settle. Storm rounds are tagged `s5*` and **excluded from
the perf medians** (their S1 latencies are artificially large by design).

Report section records facts, not thresholds: ring lag/evicted deltas,
sockets before→after, handshakes, frames healthy readers saw, anon curve
at phase marks, storm write p95, ring-bytes A→B→C.

Mechanism note: upstream gives every `/event` subscriber an **unbounded
queue** (`event.ts:25` `Queue.unbounded` + `offerUnsafe`) — a stalled
client grows memory forever. ocserve's bus is one shared ring bounded by
**count and bytes** (worst = max(32MB, largest frame)); lagging receivers
get `Lagged` → disconnect; publishers never block.

## Methodology rules

- A metric that did not run is `null`/`—`, never `0` (decidability rule from
  `harness/scripts/measure_lib.py`).
- The report shows **medians across measured rounds** plus per-arm
  **spread** `(max−min)/median` — no invented pass/fail thresholds. Set
  gates only after baselines exist.
- Warmup rounds exist because the first container start pays cold page-cache
  costs; they are recorded but excluded from medians.

## Known divergences / limitations

- Upstream **ignores the client-supplied `messageId`** on `prompt_async`
  (observed: it generates its own ULID). S3 therefore correlates on
  `sessionID`, which both arms emit.
- SSE streams are `Transfer-Encoding: chunked` on both arms; the subscriber
  dechunks properly (naive readers corrupt lines at chunk boundaries).
- The stub means **no provider variance and no token cost**, but also no
  real-provider tail latency — provider behavior is out of scope by design.
- Containers run as root (no secrets inside; ports are loopback-only).
- Host load from your other work still lands in the recorded loadavg; a
  round with unusual spread should be re-run, not averaged away.

## Antagonism record (bugs this harness hit before shipping)

1. Host-built ocserve needs `GLIBC_2.39`; bookworm has 2.36 → ocserve is
   built **inside** the container (multi-stage), not `COPY`ed from the host.
   The freeze binary only needs `GLIBC_2.17` → vendored copy is fine.
2. BuildKit `RUN --mount=type=bind` is read-only → can't create cache
   mountpoints on it (COPY into the stage instead).
3. Stub SSE without `Content-Length`/`Transfer-Encoding` is close-delimited
   in HTTP/1.1 → upstream waited for EOF forever (hung `/message` >120s).
   Fixed with chunked framing — and the `0` terminator must NOT be written
   through the chunk writer.
4. Port 8090 was already taken (a ZAP proxy) — stub host port is 18090.
5. Docker auto-created `.runs/` as root → runner couldn't write results;
   `run.sh` preflight now creates it and fixes ownership.

## Layout

```
compose.yaml           services: upstream | ocserve | stub (internal net)
Dockerfile.upstream    bookworm-slim + vendor/opencode (freeze 1.18.31)
Dockerfile.ocserve      rust:1-bookworm build → bookworm-slim runtime
Dockerfile.stub        python:3.12-slim + stub.py
config/                identical fixtures mounted into both arms
lib/sampler.py         cgroup-v2 sampler (1 Hz CSV + summaries)
lib/runner.py          one arm-run: S0→S4 → result.json
lib/report.py          medians + spread + seed-parity table → report.md
vendor/opencode        staged freeze binary (gitignored)
.runs/                 rounds, logs, report (gitignored)
```
