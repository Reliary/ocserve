# ocserve

A compatible reimplementation of the [opencode](https://github.com/anomalyco/opencode)
server API, written in Rust. It serves the same wire contract as upstream
(freeze: **1.18.31**) so existing clients work unmodified — the TUI
(`opencode attach`), the web UI, the VS Code extension, mobile apps, anything
speaking the API.

## Why

Upstream opencode is the reference implementation and is excellent. `ocserve`
exists for the cases where running it as a long-lived server is hard or
undesirable — a small VPS, an old laptop, many concurrent sessions. Measured
on one machine, same clients, same API, interleaved A/B:

| | ocserve | upstream 1.18.31 |
|---|---:|---:|
| resident memory (server) | ~110 MB | ~2.4 GB |
| database after months of use | 1.6 GB | 33 GB |
| boot | 0.6 s | 4.7 s |
| read p95 | 0–2 ms | 8–15 ms |
| mixed-read throughput | ~9,000 rps | ~450 rps |

Full method and raw results: `PERF-10X.md`, `bench/load/BASELINE.md`. These are
single-machine numbers on the hardware in those reports; re-run the harness on
your own box rather than trusting them.

## Install

```sh
cargo install ocserve          # Rust toolchain
# or grab a release binary:
#   https://github.com/Reliary/ocserve/releases
```

## Run

```sh
ocserve serve                    # listens on 127.0.0.1:4096 (upstream's default)
ocserve serve --port 4912        # pick your own
```

Then point any opencode client at it:

```sh
opencode attach http://localhost:4096     # the real TUI
# or open http://localhost:4096 in a browser — ocserve proxies the opencode
# web app same-origin, exactly like upstream's `opencode serve` does
```

Data lives in `~/.local/share/ocserve` (`OCSERVE_DATA_DIR` overrides). It reads
your existing `~/.config/opencode/opencode.json`, auth, and model cache
read-only; it never writes to them. Migrate history with
`ocserve import --source ~/.local/share/opencode/opencode.db`.

Optional systemd user service (never installed implicitly):

```sh
./scripts/install.sh          # --dry-run first if you prefer
./scripts/uninstall.sh        # keeps history by default; --purge for full wipe
```

## Compatibility, honestly

**Works today** (verified by differential replay against the frozen upstream
and a live A↔B pair harness): sessions and messages with cursor paging, the
SSE event stream, prompts and the agent loop, tools, permissions, questions,
commands, shells, todos, search, files, MCP, plugins, the `/tui/*` controller
group, auto-compaction, config hot-reload.

**Not implemented** (named, not hidden — `PLAN.md` §17): the terminal PTY
routes (`/pty/*`, used by the web UI's embedded shell), share links (hosted by
opencode.ai upstream, not by the local server), session revert/diff, and
provider OAuth login. Unmatched paths behave like upstream: they serve the
proxied web app, not a 404.

**Contract**: wire-compatible with upstream **1.18.31**. A future upstream
release may change the API; `ocserve` pins the freeze and tracks upstream drift
via a nightly watch, adopting changes deliberately rather than automatically.

## Build

```sh
cargo build --release            # requires stable Rust (1.98+)
```

## Gates

Every commit runs these; CI runs them too:

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
| `THIRD-PARTY.md` | upstream MIT attribution and the compatibility framing |

## Repository hygiene

`git config core.hooksPath .githooks` enables the commit gate (banned-string +
docs-required checks). Host services other than ocserve's own are read-only to
scripts in this repo (`AGENTS.md` §2.11).
