# ocserve

A compatible reimplementation of the [opencode](https://github.com/anomalyco/opencode)
server, written in Rust. It speaks the same HTTP and SSE wire as upstream, so
the official clients work against it unmodified: the TUI (`opencode attach`),
the web UI, and anything else that talks to the API.

Two contracts are covered:

- the **1.18.x server API** (frozen at 1.18.31), used by today's clients
- the **opencode 2 API** (`/api/*`, pinned at 2.0.28), used by the current TUI
  and web UI

## When to use this

Upstream opencode is the reference implementation and it is good. If it runs
well on your machine, use it.

ocserve exists for the cases where a long-lived server is the problem: a small
VPS, an old laptop, many concurrent sessions, months of accumulated history.

It also reclaims space. Upstream never runs an incremental vacuum, so on this
machine its 32 GB database contains about 19 GB of free pages it will not
return to the filesystem. ocserve vacuums incrementally and keeps a retention
ring, so its file tracks its data.

## At a glance

This laptop, both servers up with the same config and no traffic:

| | ocserve | upstream 1.18.31 |
|---|---:|---:|
| binary | 61 MB | 177 MB |
| server RSS, steady | 196 MB | 2,007 MB |
| boot to healthy | 0.6 s | 4.7 s |
| read p95 | 0 to 2 ms | 8 to 15 ms |

A separate idle runner (4 physical cores), read-only load test, both servers
pinned to the same two cores, no model calls, closed model:

| | ocserve | upstream 1.18.31 |
|---|---:|---:|
| throughput | 6,736 req/s | 417 req/s |
| p95 | 34.6 ms | 561.9 ms |
| peak cgroup memory | 724 MB | 3,089 MB |

Read the two tables differently. The first is one client making requests, so it
measures latency and footprint. The second saturates the route mix, so it
measures capacity and queueing. Both are single-machine results. Sources:
`bench/load/G2-BASELINE.md`, `bench/parity/`, `DIFFERENTIATION.md`. Re-run the
harness on your own box rather than trusting these.

Server RSS depends on the size of the model catalog your config loads; 196 MB
is this machine's config with a 5.3 MB catalog.

## Install

Download a release (no toolchain, checksum-verified) from
<https://github.com/Reliary/ocserve/releases>. Linux x86_64 and aarch64
tarballs, each with a `.sha256` and a combined `SHA256SUMS`.

From source (this is the supported path for current releases):

```sh
git clone https://github.com/Reliary/ocserve && cd ocserve
cargo build --release          # needs Rust 1.98+
```

**crates.io is stale on purpose.** `cargo install ocserve` resolves to
**0.3.1**; the 0.4.x and later crates are not published because `ocserve-http`
embeds the version-matched web UI and its packed payload exceeds crates.io's
10 MiB per-crate limit. A size-increase request is with the registry, and
nothing is shipped that would silently drop the air-gapped web UI just to fit.
Use a release tarball (above) or build from the tag. This note will be removed
the release the limit is raised.

From a clone, including an optional systemd user service (never installed
implicitly):

```sh
./scripts/fetch-release.sh       # download + verify the matching tarball
./scripts/install.sh --release   # place the binary and render the unit
./scripts/install.sh --dry-run   # show what it would do first
./scripts/uninstall.sh           # keeps history by default
```

## Quick start

```sh
ocserve serve                    # 127.0.0.1:4096, upstream's default port
ocserve serve --port 4912        # or pick your own
```

Then attach a client:

```sh
opencode attach http://localhost:4096   # the real TUI
```

Or open `http://localhost:4096` in a browser. ocserve serves the opencode web
app itself, embedded and version-matched to the frozen server, the same way
`opencode serve` does. It does not proxy to a remote site, so it works offline.

Data lives in `~/.local/share/ocserve` (`OCSERVE_DATA_DIR` overrides). It reads
your existing `~/.config/opencode/opencode.json`, auth, and model cache
read-only, and never writes to them. To bring history across:

```sh
ocserve import --source ~/.local/share/opencode/opencode.db
```

## Compatibility

### v1 (frozen at 1.18.31)

Verified by differential replay against the frozen upstream binary and a live
A/B pair harness. Working: sessions and messages with cursor paging, the SSE
event stream, prompts and the agent loop, tools, permissions, questions,
commands, shells, todos, search, files, MCP, plugins, the `/tui/*` controller
group, auto-compaction, config hot-reload, PTY (`/pty/*`, used by the web UI's
embedded shell), git diff browsing (`/vcs/*`), and snapshots with real revert.

API coverage is spec-driven. The frozen server ships its own OpenAPI 3.1.0
contract (162 paths, 188 operations); ocserve serves it at `GET /doc`, and a
generated guard requires every operation to be bound or explicitly cited:
**188 implemented, 0 cited, 0 gaps**.

### v2 (opencode 2)

Every v2 `/api/*` operation is bound (142 of 142), including the routes the
current TUI calls at boot, so a 2.x TUI attaches and lists sessions against
ocserve. Verified live against the real `@opencode/cli` 2.0.28 binary:

- `scripts/pair-v2-check.sh`: 28 of 28 vectors, 0 divergences
- `scripts/spec-validate-v2.sh`: 44 checked, 0 violations

The same suite also passes against 2.0.32, the newest 2.x release at the time
of writing.

Everything that exists only in the 2.x contract is behind `OCSERVE_V2=1` and
answers a clean JSON 404 without it. The gate is derived mechanically from the
two contracts, not by path prefix: `/api/*` is part of the frozen 1.18.31
contract too (58 operations, and the 1.18.31 TUI calls them), so those keep
working. What the flag actually gates is the 96 operations present in 2.0.28
and absent from 1.18.31: the run engine (inbox admission, write-ahead claims,
`content[]` projection, dual-grammar events, `log?after&follow` replay, form
service) plus the v2-only administration surface.

v2 plugins work, in both authoring shapes (promise and Effect). `session.context`,
`session.compaction`, `tool.execute.before` and `tool.execute.after` are bridged,
and `command.transform` and `skill.transform` change both the served list and
the `<available_skills>` block in the system prompt. The full capability matrix,
including what is deliberately not bridged, is in
`crates/ocserve-plugin/src/host/README-v2.md`.

Named divergences are listed in `PLAN.md` section 17. They are documented
decisions, not gaps: `D-V2-UNION` (single-port union shapes, since no
discriminator exists), `D-PROV-SECRETS` (settings never carry apiKey values),
`D-REVERT-NODELETE` (mark-only revert), `D-EVENT-ALWAYSON` (the log is always
on where upstream defaults it off).

### Not implemented

Named rather than hidden:

- hosted share links (the record is returned, but there is no remote backend)
- provider OAuth login
- `task` background mode (the tool runs foreground)

Unmatched paths get the version-matched web app, not a 404, matching upstream.

### Contract

Wire-compatible with upstream 1.18.31 and opencode 2.x. Upstream changes are
tracked by a nightly drift watch and adopted deliberately, not automatically. A
future release may change the API; when it does, `ocserve` pins the new version
and re-runs every oracle before claiming compatibility.

## Platform support

Linux x86_64 and aarch64 (glibc) are the published artifacts and remain the
reference platform.

**macOS** builds and runs, and its PTY layer is verified on a real macOS runner:
`forkpty` spawn, write, resize (the Darwin `TIOCSWINSZ` constant is checked by a
`TIOCGWINSZ` round-trip), exit code and kill. The client-facing contract is the
same one Linux serves.

**Windows** runs the server, the shell tool, the terminal routes and the frozen
wire contract. The wire is verified in a Windows 11 LTSC VM (`pair-check` 18
compared / 0 divergent against `opencode-windows-x64@1.18.31`); the terminal
routes use ConPTY and are verified on the Windows CI runner by a test that
spawns a real shell and requires its output back through the PTY. Building that
found four bugs no compile check could, all fixed (child environment, std
handles, Darwin errno values, env duplicate spelling). What is **not** there
yet: Windows-specific path normalization beyond the handle-based containment
check, long-path handling, and plugin/MCP spawning is still compile-checked only.

No performance number is claimed for macOS or Windows: every measurement in this
README comes from Linux, on the same machine, against the same freeze binary.
`PLATFORM.md` records the per-platform evidence and the open items.

## License

MIT. Upstream opencode is also MIT, and its notice is retained. See `LICENSE`
and `THIRD-PARTY.md`. This is an independent implementation, not a fork: it
never modifies upstream, and it reads upstream config files read-only.

## Build

```sh
cargo build --release            # stable Rust 1.98+
```

## Gates

Every commit runs these; CI runs them too:

```sh
cargo fmt --check && cargo clippy --workspace --all-targets -- -D warnings \
  && cargo test --workspace && ./scripts/check-guards.sh && ./scripts/check-matrix.sh
./scripts/replay-check.sh     # differential wire replay vs the frozen contract
./scripts/nightly.sh          # audit/deny/fuzz/corpus/backup/size battery
```

The differential harnesses (`replay`, `pair`, `pair-v2`, `capdiff`) need the
vendored binary for the version under test. Each has its own README under
`bench/`.

## Documentation map

| Doc | Contents |
|---|---|
| `AGENTS.md` | binding rules, commit and branch policy, working agreement |
| `PLAN.md` | scope, contract freeze, architecture, API surface, divergences |
| `V2-PLAN.md` | opencode 2 engine: inbox, claims, union wire, dual events, log replay |
| `GAPS-PLAN.md` | gap closure: what was built, what stays diverged and why |
| `crates/ocserve-plugin/src/host/README-v2.md` | v2 plugin capability matrix |
| `STORAGE.md` | SQLite pragmas, schema and blob rules, FTS5 design |
| `MEMORY.md` | memory budget lines, allocator settings, allocation rules |
| `SRE.md` | boot checks, metrics, config knobs, service and rollback |
| `TESTING.md` | test levels, traceability, anti-theater rules |
| `COMPACTION.md` | auto-compaction: upstream algorithm, wire surfaces, design |
| `SNAPSHOT.md` | snapshots, revert/unrevert, session diff, `/vcs/apply` |
| `PERF-10X.md` | performance program: target, acceptance, evidence |
| `bench/load/`, `bench/parity/`, `bench/pair/`, `bench/capdiff/` | benchmark harnesses and their results |
| `DIFFERENTIATION.md` | what is built differently from upstream, and what was killed |
| `TRACEABILITY.md` | requirement to test to evidence matrix |
| `THIRD-PARTY.md` | upstream MIT attribution and compatibility framing |

## Repository hygiene

```sh
git config core.hooksPath .githooks    # enables the commit gate
```

Host services other than ocserve's own are read-only to scripts in this repo
(`AGENTS.md` section 2.11).
