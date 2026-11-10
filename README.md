# ocserve

An independent Rust implementation of the
[opencode](https://github.com/anomalyco/opencode) server. It serves the same
HTTP and SSE API, so the official clients work against it unchanged: the TUI
(`opencode attach`), the web app, or any other client written for that API.

Two contracts are covered, each checked against the real upstream binary:

- **v1**, frozen at 1.19.5: 188 of 188 operations, used by current clients.
- **v2** (`/api/*`), pinned at 2.2.5: 169 of 169 operations, including every
  route the 2.x TUI calls while booting.

The 123 operations that exist only in v2 are opt-in. `/api/*` also carries 46
operations that v1 clients use, so the flag gates the v2-only set rather than
the path prefix. Run `OCSERVE_V2=1` to serve it. See
[v2](#v2-opencode-2) for the details.

## When to use this

Upstream opencode is the reference implementation and it is good. If it runs
well on your machine, use it.

ocserve exists for the cases where a long-lived server is the problem: a small
VPS, an old laptop, many concurrent sessions, months of accumulated history.

It also reclaims disk space. Upstream never runs an incremental vacuum, so on
this machine its 32 GB database holds about 19 GB of free pages it will not
return to the filesystem. ocserve vacuums incrementally and keeps a retention
ring, so the file size tracks the data.

## At a glance

One client, no traffic, both servers on the same machine with the same config.
Measured against 1.18.31; the 1.19.5 wire is identical apart from two schema
descriptions, so these numbers carry over, but they were not re-run on 1.19.5.

| | ocserve | upstream 1.18.31 |
|---|---:|---:|
| binary | 68 MB | 177 MB |
| server RSS, steady | 227 MB | 3,712 MB |
| boot to healthy | 0.6 s | 4.7 s |
| read p95 | 0 to 2 ms | 8 to 15 ms |

A separate idle runner (4 physical cores), read-only load test, both servers
pinned to the same two cores, no model calls, closed model:

| | ocserve | upstream 1.18.31 |
|---|---:|---:|
| throughput | 6,736 req/s | 417 req/s |
| p95 | 34.6 ms | 561.9 ms |
| peak cgroup memory | 724 MB | 3,089 MB |

The two tables measure different things. The first is one client making
requests, so it shows latency and memory. The second saturates the route mix,
so it shows capacity and queueing. Both are single-machine results, and the
second depends on how the servers were pinned to cores. Sources:
`bench/load/G2-BASELINE.md`, `bench/parity/`, `DIFFERENTIATION.md`. Run the
harness on your own machine instead of trusting these.

Server RSS scales with the model catalog your config loads. These figures were
re-measured on 2026-11-10 with this machine's config and a 5.3 MB catalog; the
upstream figure is that process's resident set at the time, which had been up
for days with its own history loaded.

## Install

### cargo

```sh
cargo install ocserve
```

This compiles the whole crate family from crates.io, including the embedded web
UI packs, so you get the same feature set as the release tarballs. The bytes
differ because it is built on your machine.

It needs a C toolchain, because BoringSSL is compiled from source: `cmake`,
`perl`, a C compiler and `pkg-config`. On Debian and Ubuntu install
`build-essential cmake perl pkg-config`; on macOS run
`xcode-select --install` and install `cmake`. Rust 1.98 or newer.

### Prebuilt binary

Download a checksum-verified tarball (no toolchain, no build) from
<https://github.com/Reliary/ocserve/releases>: Linux x86_64 and aarch64 (glibc),
Linux x86_64 (static musl), macOS x86_64 and aarch64, Windows x86_64 and
aarch64. Each tarball has its own `.sha256`, and the release carries a combined
`SHA256SUMS`.

```sh
tar xzf ocserve-v0.7.0-linux-x86_64.tar.gz
./ocserve-v0.7.0-linux-x86_64/ocserve --version
```

### From source

```sh
git clone https://github.com/Reliary/ocserve && cd ocserve
cargo build --release
```

From a clone, including an optional systemd user service (never installed
implicitly):

```sh
./scripts/fetch-release.sh       # download + verify the matching tarball
./scripts/install.sh --release   # place the binary and render the unit
./scripts/install.sh --dry-run   # show what it would do first
./scripts/uninstall.sh           # keeps history by default
```

`scripts/fetch-release.sh --platform linux-x86_64-musl` selects the static
build, and a positional tag (`scripts/fetch-release.sh v0.7.0`) pins a version.
`install.sh --bin <path>` installs a binary you already have (for example one
you built) and renders the unit to point at it. `install.sh --bin-only` is the
different, common case: place the binary and leave systemd untouched.

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

That takes the 20 most recent sessions (`--limit N` changes it). Adding
`--session <id>` (repeatable) adopts named sessions instead, which is safe
while `ocserve serve` is running:

```sh
ocserve import --source ~/.local/share/opencode/opencode.db \
  --session ses_abc123 --session ses_def456
```

Add `--force` to replace a session that is already there (`--force` requires at
least one `--session`, so a destructive import is never implicit).

Stopping the server does not by itself make an import fast: the index strategy
is chosen from the target's contents, not from whether a server is running. A
populated data dir always takes the per-row trigger path unless you ask for
`--bulk`:

```sh
# stop the service first (the forwarder is independent and may stay up)
systemctl --user stop ocserve.service
ocserve import --source ~/.local/share/opencode/opencode.db \
  --session ses_abc123 --bulk
systemctl --user start ocserve.service
```

`--force` takes a restorable `VACUUM INTO` copy in the data dir before it
deletes anything, so a failed re-import never leaves you without the old
session.

`--bulk` replaces per-row indexing with one index rebuild. It is clearly faster
for large sessions and can be slower for small ones (the rebuild has a fixed
cost), so it is a choice, not a default. Above 10,000 source parts the import
refuses before writing anything and asks you to pick `--bulk` (server stopped)
or `--allow-slow-live` (server keeps running).

## Configuration

`ocserve doctor --env` prints every `OCSERVE_*` variable, its default, and the
values set in your shell. `ocserve env-reference` prints the same table as
[`docs/ENVIRONMENT.md`](docs/ENVIRONMENT.md), which is generated from the code
so it cannot go stale. Most installations need no environment variables at all:
the defaults are the documented behaviour, and only kill switches (`=0`) and
tuning knobs are there when you need them.

## Compatibility

### v1 (frozen at 1.19.5)

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

Every v2 `/api/*` operation is bound (169 of 169), including the routes the
current TUI calls while booting, so a 2.x TUI attaches and lists sessions.
Checked live against the real `@opencode/cli` 2.2.5 binary:

- `scripts/pair-v2-check.sh`: 55 vectors, 0 divergences
- `scripts/spec-validate-v2.sh`: 38 checked, 0 violations

Everything that exists only in the 2.x contract is behind `OCSERVE_V2=1` and
answers a clean JSON 404 without it, so pointing this at an existing 1.x install
changes nothing until you ask for it:

```sh
OCSERVE_V2=1 ocserve serve --port 4096
```

The gate is derived mechanically from the two contracts, not by path prefix:
`/api/*` is part of the frozen 1.x contract too (58 operations, and the
1.x TUI calls them), so those keep working. What the flag actually gates is
the 123 operations present in 2.2.5 and absent from the 1.x contract: the run
engine (inbox admission, write-ahead claims, `content[]` projection,
dual-grammar events, `log?after&follow` replay, form service) plus the v2-only
administration surface.

v2 plugins work, in both authoring shapes (promise and Effect). `session.context`,
`session.compaction`, `tool.execute.before` and `tool.execute.after` are bridged,
and `command.transform` and `skill.transform` change both the served list and
the `<available_skills>` block in the system prompt. The full capability matrix,
including what is deliberately not bridged, is in
`crates/ocserve-plugin/src/host/README-v2.md`.

Every divergence is one row in the single ledger at `PLAN.md` section 18
(`D-*`), with the reason and the evidence behind it. They are documented
decisions, not gaps. The ones a client author will notice first:

- `D-V2-UNION`: one port serves both shapes, because no client sends a
  version discriminator
- `D-PROV-SECRETS`: settings never carry API-key values
- `D-REVERT-NODELETE`: revert restores files and marks history; it does not
  delete rows
- `D-EVENT-ALWAYSON`: the durable log is on by default where upstream needs
  `persist: true`
- `D-TASK-BACKGROUND`: `task` runs in the foreground

### Not implemented

Not implemented:

- hosted share links (the record is returned, but there is no remote backend)
- provider OAuth login
- `task` background mode (the tool runs foreground)
- two websearch providers behind `D-WEBSEARCH-PARTIAL` (Tavily and Exa work)

Unmatched paths get the version-matched web app, not a 404, matching upstream.

Web UI assets are embedded, not fetched: the release ships the bundle from the
same upstream version it targets, so a server with no outbound network still
serves the UI. `OCSERVE_DISABLE_EMBEDDED_WEB_UI=1` switches to proxying the
hosted bundle instead.

### Contract

Wire-compatible with upstream 1.19.5 and opencode 2.2.5. A nightly drift watch
reports when upstream publishes something newer; adopting it is a manual step
that re-runs every oracle before any compatibility claim is made.

## Platform support

Linux x86_64 and aarch64 (glibc) are the published artifacts and the reference
platform.

**macOS** builds and runs. Its PTY layer is verified on a real macOS runner:
`forkpty` spawn, write, resize (the Darwin `TIOCSWINSZ` constant is checked with
a `TIOCGWINSZ` round-trip), exit code and kill. It serves the same
client-facing contract as Linux.

**Windows** runs the server, the shell tool, the terminal routes and the frozen
wire contract. The wire was verified in a Windows 11 LTSC VM: `pair-check` 18
compared, 0 divergent against `opencode-windows-x64@1.18.31`. That run predates
the move to 1.19.5 and has not been repeated, because the VM was torn down. The terminal
routes use ConPTY and are covered on the Windows CI runner by a test that
spawns a real shell and reads its output back through the PTY. That work found
four bugs a compile check cannot catch, all fixed: the child environment, the
standard handles, Darwin errno values, and duplicate environment names.

Not yet done on Windows: path normalization beyond the handle-based containment
check, long-path handling, and plugin/MCP spawning (compile-checked only).

No performance numbers are claimed for macOS or Windows. Every measurement in
this README comes from Linux, on one machine, against the same frozen binary.
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

The full index is in `docs-README.md`. The short version:

| You want to | Read |
|---|---|
| install, or decide whether this fits | `README.md`, `CHANGELOG.md` |
| expose it beyond loopback | `SECURITY.md` |
| understand the wire contract and what differs | `PLAN.md` (divergence ledger in section 18) |
| work on storage, memory, or the service | `STORAGE.md`, `MEMORY.md`, `SRE.md` |
| change behaviour without breaking something | `TESTING.md`, `TRACEABILITY.md`, `AGENTS.md` |
| work on a subsystem | `COMPACTION.md`, `SNAPSHOT.md`, `PLATFORM.md`, `V2-PLAN.md` |
| run the compatibility or load harnesses | `bench/plugins/README.md`, `bench/load/README.md` |

## Heavy builds

A build started inside a tool call is an external workload. The service keeps it
contained but does not schedule it. Run heavy Rust work in its own scope:

```sh
systemd-run --user --scope --collect \
  -p MemoryHigh=4G -p MemoryMax=6G -p CPUQuota=800% -p Nice=10 \
  --working-directory="$PWD" \
  cargo build --release
```

That scope sits outside `ocserve.service`, so it does not count against the
service's memory and the service cannot kill it. For a standing policy, put
those properties in `~/.config/systemd/user/ocserve-build.slice` and pass
`--slice=ocserve-build.slice`. Do not background a build inside a tool call
(`nohup cargo build &`): it stays in the tool class and is capped with it.

The service itself weights CPU so the API wins over its child classes and
caps each class separately (see `SRE.md` section 5.5). It does not set
`CARGO_BUILD_JOBS`, `RAYON_NUM_THREADS` or a build CPU quota.

## Unmanaged resource policy

`scripts/install.sh --no-resource-limits` installs the unit without memory
caps and turns off the child partition and sidecar recycle, leaving the
operator to own memory policy. The default install is managed.

## Repository hygiene

```sh
git config core.hooksPath .githooks    # enables the commit gate
```

Host services other than ocserve's own are read-only to scripts in this repo
(`AGENTS.md` section 2.11).

