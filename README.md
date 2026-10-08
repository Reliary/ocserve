# ocserve

A compatible reimplementation of the opencode server API, written in Rust.
`ocserve` serves the same wire contract as upstream opencode (freeze:
**1.18.31**) so existing clients — the TUI, mobile apps, anything speaking the
API — work unmodified. It is built for a small, bounded footprint (memory,
storage, boot time) and is verified by differential replay against the frozen
upstream and by an interleaved A/B load harness.

Not a fork: this repository shares no code with upstream. It is a
from-scratch, wire-compatible replacement — see `THIRD-PARTY.md` for the MIT
attribution notice and the compatibility framing.

## Build & run

```sh
cargo build --release
./target/release/ocserve serve --port 4912      # foreground; data in ~/.local/share/ocserve
```

Optional systemd user service (never installed implicitly):

```sh
./scripts/install.sh          # --dry-run first if you prefer
./scripts/uninstall.sh        # keeps history by default; --purge for full wipe
```

## Gates

```sh
cargo fmt --check && cargo clippy --workspace --all-targets -- -D warnings \
  && cargo test --workspace && ./scripts/check-guards.sh && ./scripts/check-matrix.sh
./scripts/replay-check.sh     # differential wire replay vs the frozen contract
./scripts/nightly.sh          # audit/deny/fuzz/corpus/backup/size battery
```

## Documentation map

| Doc | Contents |
|---|---|
| `AGENTS.md` | binding instructions: hard rules, commit/branch policy, working agreement |
| `PLAN.md` | scope, contract freeze, architecture, API surface, milestones, standards |
| `STORAGE.md` | SQLite pragmas, schema/blob rules, FTS5 design, storage gates |
| `MEMORY.md` | memory budget lines, allocator/runtime settings, allocation rules |
| `SRE.md` | boot checks, metrics, config knobs, CPU policy, service/rollback |
| `TESTING.md` | test levels, traceability, anti-theater rules, adversarial program |
| `COMPACTION.md` | auto-compaction: upstream algorithm, wire surfaces, design |
| `DIFFERENTIATION.md` | what is built differently from upstream, and the killed ideas |
| `PERF-10X.md` | performance program: target, acceptance, evidence, kill-switches |
| `TRACEABILITY.md` | requirement ↔ test ↔ evidence matrix |

## Repository hygiene

`git config core.hooksPath .githooks` enables the commit gate (banned-string +
docs-required checks). Host services other than ocserve's own are read-only to
scripts in this repo (`AGENTS.md` §2.11).
