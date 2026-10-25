# Documentation

Docs are the specification (see `AGENTS.md` §1). Two rules make that true
rather than aspirational: a doc that is not in this index is not authoritative,
and `scripts/check-docs.py` mechanically rejects dead paths, dangling guard-rule
references and stale divergences on every commit.

## Start here

| Doc | Read it when |
|---|---|
| `README.md` | you are installing or deciding whether to use ocserve |
| `CHANGELOG.md` | you are upgrading, or checking what a version actually fixed |
| `SECURITY.md` | you are exposing the server beyond loopback, or reporting a vulnerability |
| `CONTRIBUTING.md` | you are sending a change |
| `THIRD-PARTY.md` | you need the upstream attribution and compatibility framing |

## Specification

| Doc | Authority |
|---|---|
| `PLAN.md` | scope, wire contract and freeze, architecture, API surface, milestones, divergence ledger (§17) |
| `STORAGE.md` | SQLite pragmas, schema and blob protocol, FTS5 design, storage gates |
| `MEMORY.md` | memory budget lines, allocator rules, allocation gates |
| `SRE.md` | boot checks, metric-per-KPI table, config knobs, CPU policy, CI/CD, systemd, runbook |
| `docs/ENVIRONMENT.md` | every `OCSERVE_*` variable, generated from the code contract (rule 58) |
| `TESTING.md` | test levels and techniques, traceability, anti-theater rules, adversarial program, exit criteria |
| `TRACEABILITY.md` | requirement to test to evidence matrix |

## Subsystems

| Doc | Scope |
|---|---|
| `COMPACTION.md` | auto-compaction: upstream algorithm, projection, trigger math, perf, divergences |
| `SNAPSHOT.md` | file snapshots, revert/unrevert, `/vcs/apply`, side-effect rules |
| `PLATFORM.md` | macOS and Windows: measured ground truth, seams, phases, divergences |
| `DIFFERENTIATION.md` | what we build beyond upstream and the evidence for each claim |
| `PERF-10X.md` | performance program: targets, acceptance, fixes, kill switches |
| `V2-PLAN.md` | v2 engine internals: inbox, coordinator, claims, union wire, dual events, log |
| `V2-PARITY-PLAN.md` | v2 parity phases and the maintenance machine (`scripts/bump.sh`) |
| `V2-EXECUTION-PLAN.md` | the decisions and phases behind v2 plugins, auth and mutation testing |
| `GAPS-PLAN.md` | closed/remaining gaps with their acceptance criteria |

## Evidence and harness reports

| Path | Contents |
|---|---|
| `bench/plugins/README.md` | real-plugin corpus: method, result classes, sandbox bounds, limits |
| `bench/plugins/demand.json` | declared plugin surface per hook, with bound/diverged status (guard 50) |
| `bench/BUMP-MACHINERY-TEST.md` | upgrade-machinery shakedown against 2.0.32 (report only) |
| `bench/load/BASELINE.md` | performance baseline and withdrawn/corrected figures |
| `bench/perf/PHASE1-ATTRIBUTION.md` | where the read-path time actually went |
| `bench/prompt-window/REPORT.md` | bounded prompt-load equivalence proof, timings, and the negative controls (K-BOUNDED-PROMPT-LOAD) |
| `bench/load/README.md` | k6 load suite: methodology, pinning, thresholds |

## Conventions

- One doc owns each decision. If two disagree, the more specific one wins and
  the other is edited in the same commit (AGENTS §1).
- Status lines carry a date. A status that is not current is a defect, not a
  style issue.
- A capability that is not implemented is written as `D-<NAME>` in `PLAN.md §17`
  or `README-v2.md`, never as prose that implies it works.
- No em dashes in `README.md`, `SECURITY.md`, `CONTRIBUTING.md`; the linter
  enforces it.
