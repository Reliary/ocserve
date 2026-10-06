# bench/load — k6 load suite: refine vs upstream freeze (MANUAL)

Pre-registration. Scenarios and claim rules below are fixed BEFORE any
gated number is produced; results never outrun their evidence class.

## What this measures (L1 — built)

Server-side **resource efficiency, throughput and concurrency breakpoints**
of *both* servers under **identical read load**:

- **`read-hot.js`** — ramping-VU concurrency ladder (`LOAD_TARGETS`,
  default `10,25`; ramp 20 s + hold 45 s per step): concurrent *connections*
  capacity curve. Two modes per arm:
  - `spread` — each VU pinned to **its own session** from the seeded pool
    (200 sessions + the 32k-message deep session, overweighted ×5) → the
    **concurrent-sessions** number
  - `hot` — every VU on the deep session → contention on the heaviest row
- **`arrival.js`** (opt-in `LOAD_ARRIVAL=1`) — constant offered rate
  (`LOAD_RPS`) → achieved-vs-offered = queueing signal
- Route mix (`mix.js`): session list, message page + `X-Next-Cursor` page,
  config, agent, command, file list, `POST /session/search`, session status
- **Zero LLM-provider traffic by design** — no prompts exist in the
  measured window; the deterministic dummy (stub) belongs to L2 only
  (`L2-DESIGN.md`, pre-registered, not built)

## CPU pinning (equal resources, big/little aware)

Both arms are pinned with `taskset` to **2 whole physical cores each**
(both SMT threads, disjoint sets) of the **same CPU class**, and the k6
client gets its own 2 physical cores — so client load never steals arm
cores and neither arm can migrate between big and little cores
(Meteor Lake: `cpu_core` 0-11 homogeneous big / `cpu_atom` 12-21 mixes
regular E with LP E-cores, which is why **big is the default class** —
same-class fairness is structural, not statistical). Allocation is
printed in the report. Knobs:

- `LOAD_ARM_CLASS=big|atom` (default `big`; atom = opt-in, see LP caveat)
- `LOAD_REF_CORES` / `LOAD_FREEZE_CORES` / `LOAD_K6_CORES` — explicit
  lists override detection entirely

Caveat kept honest: other processes (browser, TUI) are *not* moved off
these cores — we never touch your tasks — so the quiet-host gate, round
ordering and recorded loadavg still stand. Pinning removes scheduler and
big/little variance; it does not create a dedicated machine.

## Isolation (hard rules)

- k6 targets **only** `127.0.0.1:4930/4931` (fixture arms). Live `:4912`
  / `:4901` are health-asserted before/after; `check-guards.sh` rule 13
  statically bans live ports on any k6-targeting line.
- Arms boot in fixture homes (`plugin: []`, `mcp: {}` — bare cores),
  `REFINE_LEGACY_SYNC=0`, models fetch off. Legacy db is a **read-only**
  snapshot source (`mode=ro`).
- **Fixture data is your real session content**: `bench/load/.fixtures/`
  is gitignored and ephemeral; remove with `scripts/load-test.sh --clean`.
  Reports contain metrics only.
- **Sequencing**: never during a parity run (the harness refuses if the
  parity runner/stack is active) or the nightly window (~06:00).

## Fixture (equivalent by construction)

`make_fixture.py` builds ONE subset snapshot from the legacy db —
`LOAD_SESSIONS` (default 200) most-recent non-archived sessions **+ the
deepest session** — then both arms derive from it: freeze gets it as its
native `opencode.db`; refine gets `refine import`. The harness asserts
**equal `GET /session` counts** or exits infra. Disk only (never tmpfs).
Scoping (lever 3, proven empirically): freeze lists `listByProject(ctx.project.id)`
and project ids are git-derived (stored refine-project id = commit `8b87603…`),
so boot-time cwd can never match stored ids (first run: freeze=0) — the builder
rewrites all selected sessions to the stable `global` project and boots both
arms at `cwd=/`. The count-equality assertion still gates every run.

## Threshold discipline (no invented numbers)

1. **Baseline run** (default): `--no-thresholds` — informational only.
2. Derive from the baseline with declared intent:
   `bench/load/thresholds.json` =
   `{"err_rate_max": 0.005, "p95_ms_max": max(2*baseline_p95, baseline+30)}`
3. **`GATED=1`** runs enforce it (exit 1 on breach).

**Claim classes** (printed in every report): achieved-capacity and
error-rate are valid at n=1; arm-to-arm **latency deltas are INDICATIVE
until ROUNDS≥3** (the standing 2.7× variance rule).

## Usage

```sh
scripts/load-test.sh --self-test        # threshold/exit wiring (no server)
scripts/load-test.sh                    # baseline A/B (informational)
GATED=1 scripts/load-test.sh            # enforce committed thresholds.json
ROUNDS=2 LOAD_TARGETS=10,25,50 LOAD_ARRIVAL=1 LOAD_RPS=200 \
  LOAD_SESSIONS=200 scripts/load-test.sh
scripts/load-test.sh --clean            # drop fixtures (real data)
```

Exit codes: `0` green/baseline · `1` gated breach · `2` infra (gate,
collision, boot, count-mismatch, live-service health) · `3` self-test.

Reports: `bench/load/.runs/<ts>/report.md` (k6 summary-exports +
server-side sampler peaks + loadavg context).
