# PLATFORM.md — macOS and Windows support

Status: **executing**. Owner: platform workstream. Precedence: `TESTING.md` gates win,
then `PLAN.md`, then this document (AGENTS §1).

The goal is not "it compiles on three platforms". The goal is: the same wire contract,
the same behaviour, the same bounded-resource discipline, and an honest statement of what
was measured where. Anything weaker is recorded as a named divergence.

---

## 1. Ground truth (measured, not assumed)

### 1.1 Current build reality

`release.yml` declares six targets. The v0.4.0 run produced:

| Target | Result |
|---|---|
| `x86_64-unknown-linux-gnu` | success |
| `aarch64-unknown-linux-gnu` | success |
| `x86_64-apple-darwin` | **failure** |
| `aarch64-apple-darwin` | **failure** |
| `x86_64-pc-windows-msvc` | **failure** |
| `aarch64-pc-windows-msvc` | **failure** |

Non-Linux builds are `continue-on-error`, so nothing has ever gated them. That is the
baseline this document replaces.

### 1.2 Why each platform fails

**macOS — the PTY shim cannot compile.** `crates/ocserve-pty/src/unix.rs` uses five
primitives that libc 0.2.191 does not declare for `apple`:

| Symbol | Linux | macOS |
|---|---|---|
| `posix_openpt` | yes | **absent** |
| `grantpt` / `unlockpt` | yes | **absent** |
| `ptsname_r` | yes | **absent** |
| `TIOCSCTTY` | yes | **absent** |
| `TIOCSWINSZ` / `TIOCGWINSZ` | yes | **absent from the crate** (Darwin ABI value is `0x80087467`, stable) |
| `openpty` / `login_tty` / `forkpty` | (linux has openpty) | **present** |

So macOS needs a second implementation, not a `cfg` tweak.

**Windows — two hard compile errors plus a runtime cliff.**

Hard errors (ungated `std::os::unix`):
- `crates/ocserve-cli/src/partition.rs:20` — `use std::os::unix::fs::MetadataExt;` (dead
  import; the file references none of its methods)
- `crates/ocserve-cli/src/models_dev.rs:294` — unix `PermissionsExt` inside a function body

Runtime cliffs that no compiler catches:
- `ocserve-store/src/blob.rs:131` — `fs::File::open(dir)` **with `?`** for the directory
  fsync. Opening a directory as a file fails on Windows, so **every blob write fails**.
  The trailing comment (`not supported on all FS; best-effort`) is attached to the wrong
  statement: the sync is best-effort, the open is treated as mandatory.
- `sh` is load-bearing in five production sites (`core/shell.rs:59`, `mcp:199`,
  `plugin:279`, `tools:346`, plus `tools:232` defaulting to `"sh"`). Windows has no `sh`;
  MCP servers and the plugin sidecar become unstartable, which removes plugins and MCP.
- `OOM_CHILD_WRAPPER` is POSIX shell (`2>/dev/null`, `[ -n "$X" ]`, `exec "$@"`) and is
  passed through `sh -c` for both MCP and plugin spawns. On Windows it is a syntax error.
- `plugin::which` does `dir.join(prog)` with no `EXE_SUFFIX`; `mcp` spawns `prog` directly.
  Neither finds `bun.exe`.
- Tool timeout kill is `child.kill()` + `#[cfg(unix)] killpg`. On Windows grandchildren
  survive — the 2026-10-08 zombie-tool bug, resurrected.
- 21 `$HOME` reads with `"."` / `"/"` fallbacks. On Windows `HOME` is usually unset, so the
  data dir lands in the process CWD and the config lookup silently returns defaults. Not a
  crash: a data-location footgun.

### 1.3 What is already portable (verified, not assumed)

- The **hot path** — axum → store → splice → wire memos — touches no OS API.
  `crates/ocserve-store/src/lib.rs`: 0 platform calls. `compress.rs`: 0. `pragma.rs`: one
  `cfg(unix)` chmod. `blob.rs`: the fsync issue above.
- TCP, TLS (rustls + `ring`), JSON, zstd, and the SQLite bundled build are all
  cross-platform.
- `libc::killpg` and `process_group(0)` exist on macOS; only Windows lacks them.
- `df -k` exists on macOS; on Windows the boot check already degrades to a `warn`
  (it is a non-fatal branch, verified by reading the match arms).

### 1.4 Platform freeze binaries exist, so differential oracles are obtainable

npm registry metadata, read directly:

| Package | Size (unpacked) | Notes |
|---|---|---|
| `opencode-windows-x64@1.18.31` | 179,998,388 B | `os: ["win32"]`, signed |
| `@opencode/cli-windows-x64@2.0.28` | 212,916,854 B | `os: ["win32"]`, `cpu: ["x64"]`, signed |
| `opencode-darwin-arm64@1.18.31` | declared optional dep of `opencode-ai@1.18.31` | v1 arm64 client |

Consequence: "ported but not differentially verified" is a choice, not a constraint. If we
ship a platform without pair-check there, it gets a `D-` row saying so.

### 1.5 What cannot be verified from this machine

- Any macOS runtime behaviour. No runner, no target installed, no way to execute. Static
  symbol checks are done; **behavioural claims require a macOS CI runner** and are
  pre-registered as Phase P entrance criteria.
- Windows `std::fs` semantics. `rust-src` is not installed, so `File::open` on a directory
  and `rename`-over-open behaviour are reasoned from Win32 documented behaviour
  (`ERROR_ACCESS_DENIED`, `MOVEFILE_REPLACE_EXISTING` + share modes), and each is covered
  by a test that must run on a Windows runner before the claim ships.

---

## 1.4 Windows PTY (ConPTY) and tree-kill (done, runner-verified)

`crates/ocserve-pty/src/windows.rs` implements the PTY on ConPTY:
`CreatePseudoConsole` + `CreateProcessW` with `PROC_THREAD_ATTRIBUTE_PSEUDOCONSOLE`,
because `std::process::Command` cannot express either the attribute list or a
console-backed child. Three details that are the whole difficulty:

| Detail | Handling |
|---|---|
| Environment block | UTF-16, sorted case-insensitively, double-NUL terminated. `build_env_block`, unit-tested for sort, terminator, last-wins and invalid names. |
| Command line | `CreateProcessW` takes one string. Every argument goes through `quote_arg` (MSVCRT rules) so the child cannot re-split an argument. |
| Shutdown | `ClosePseudoConsole` blocks while output is pending. The reader is a `PeekNamedPipe` poller that never blocks, so close cannot deadlock. |

Tree-kill: Windows has no process groups, so the bash tool spawns the child
**suspended** inside a Job Object carrying `PROC_THREAD_ATTRIBUTE_JOB_LIST`
(`crates/ocserve-tools/src/win_job.rs`). The child cannot run, and therefore
cannot fork a grandchild, before it is in the job; `KILL_ON_JOB_CLOSE` also
means a crashed server cannot leak tool processes, which `taskkill /T` cannot
provide. If assignment or resume fails, the child is killed and the spawn
errors — never a silently uncontained process.

Verified by `cargo test -p ocserve-pty` on both `platform check` runners. The
test spawns a real shell, writes a command and requires the marker to come back
through the reader; the negative control (reader appends nothing) fails in
10 s with the captured output printed.

Remaining, recorded: interactive resize fidelity (`mode con` in a real
console) is a VM-only check, because the CI runner has no console to inspect.

## 1.3 Path containment (Phase 1, done)

There were **three** containment implementations: `lib.rs::scope_path` (lexical
fallback), `v2.rs::resolve_within` (deepest-existing-ancestor), and
`v2.rs::fs_read` (inline `canonicalize` + `starts_with`). All three compared
strings, so all three had the check/use gap: a component could be swapped for a
symlink between canonicalize and open.

Now there is one: `crates/ocserve-http/src/containment.rs`.

| Platform | Mechanism |
|---|---|
| Linux, relative path | `openat2(RESOLVE_BENEATH)` — the kernel refuses any resolution outside the base, including absolute symlink targets. There is no window to race. |
| Linux, absolute path | open, then `readlink /proc/self/fd/N` and verify; the fd pins the inode. |
| macOS | open, then `fcntl(F_GETPATH)` and verify. |
| Windows | open, then `GetFinalPathNameByHandleW` and compare with `CompareStringOrdinal` (the kernel's collation, not `to_lowercase`). |

Creation goes through `create_within`, which walks components with
`mkdirat`+`openat2` from the previous verified fd, so `create_dir_all` on an
unresolved path (the F6 escape) can no longer happen.

Named residual: directory entries are read from the verified handle on Unix; on
Windows std has no directory-fd iteration, so entries are read from the verified
path (the directory itself is handle-verified). Closing that needs
`GetFileInformationByHandleEx(FileIdBothDirectoryInfo)` and belongs to Phase W.

Guard rule 43 makes a fourth string-based copy a build failure, with a planted
control. Its first run found the third copy, which is why the rule exists.

## 2. Antagonism ledger

Each entry is a design decision attacked before code. Status: `resolved` (design changed),
`accepted` (risk accepted with a named D-row), or `parked`.

| # | Attack | Resolution | Status |
|---|---|---|---|
| A1 | "Windows support" has no oracle: every differential harness drives the **Linux** freeze binary, so Windows parity would be unmeasured | Acquire per-platform freeze binaries (§1.4); run `pair-check` / `spec-validate` on each platform against that platform's binary. Until then: `D-PLATFORM-UNVERIFIED` | resolved |
| A2 | "Same performance" is unprovable across platforms: ClusterBench (arXiv:2608.10956) measures **5%** variation across spec-identical nodes, so a cross-platform number is noise | Never publish a cross-platform perf comparison. Per platform: measure against *that platform's* freeze binary. Portable yardstick: allocation bytes/request (arXiv:2203.04394), which is deterministic and OS-independent *if* measured | resolved |
| A3 | Replacing `sh -c` silently changes tool semantics; upstream v2 selects `COMSPEC`/PowerShell and v1 has `cygpath`/`Shell.ps` branches | Three shell families (`posix`, `cmd`, `pwsh`), no cross-dialect translation. Smoosh (arXiv:1907.05308) shows even POSIX-aspiring shells disagree; translating quoting is unsound | resolved |
| A4 | Process-tree kill does not port: `killpg` is Unix-only | Job Object on Windows with `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE` (survives server crash, unlike `taskkill`); pre-registered fallback `taskkill /T /F` if Job Objects fail the spike | resolved |
| A5 | Path containment will be wrong on Windows: `canonicalize` yields `\\?\` verbatim paths; NTFS is case-insensitive. CVE-2021-21300 (arXiv:2211.16735) is exactly this class at the app layer | `scope_path` gains a platform normalizer: strip verbatim prefix, compare case-insensitively on case-insensitive filesystems, reject trailing dot/space and 8.3 aliases. Property-tested per platform | resolved |
| A6 | The allocator story is glibc-only: `MALLOC_ARENA_MAX=1` + `malloc_trim` measured 237→91 MB | Measure per platform before porting. If retention is already flat, `trim_heap()` stays a documented no-op per platform rather than gaining an unjustified `malloc_zone_pressure_relief` | resolved |
| A7 | `continue-on-error` means the gate rots again | Flip to hard-required in the same commit that makes the target pass | resolved |
| A8 | Replacing `grep` is a *semantic* change, not a portable-executable swap | Measured: GNU `grep -E '\d+'` returns **no match** (rc 1) where ripgrep matches. Upstream 1.18.31 uses ripgrep (`grep.ts:5`). So the current behaviour is a silent-miss bug. Replacement is gated by a corpus diff proving *intended* widening | resolved |
| A9 | `grep`→`regex` does not fix gitignore parity: upstream is gitignore-aware, we pass `--exclude-dir` | Two separate rows: `D-FIND-IGNORE` (parity gap) stays open; the engine change is scoped to dialect + Windows availability | accepted |
| A10 | Per-request `cargo build` for Windows cannot be validated without `rust-src` | `cargo check --target x86_64-pc-windows-msvc` type-checks the whole workspace without std source; it is a real signal for the two compile errors. Semantics still need a runner | resolved |
| A11 | Windows has no `sh`, so Phase S alone produces nothing shippable | Accepted and stated: Phase S is a gate-enabler, not a deliverable. Phases G/W produce the usable increment | accepted |
| A12 | Long paths: Windows `MAX_PATH` is 260 unless the process is `longPathAware`; we do nothing | Scope `/file` and `/find` to reject paths > 32k and rely on `core.longpaths=true` for git; add a named divergence `D-WIN-LONGPATH` rather than an incomplete fix | parked |
| A13 | Network filesystems break WAL on Windows (documented SQLite limitation) | Detect non-local data dir at boot and fail fast with a named message; no silent fallback to `journal_mode=DELETE` (it would change durability semantics) | resolved |
| A14 | `fs::rename(tmp, final)` fails over an open handle on Windows; blobs are content-addressed so a re-publish can collide | If the final path already exists, skip the rename **but still fsync the directory**; content-addressing makes the existing file authoritative | resolved |
| A15 | Cross-platform tests would triple CI cost | Only `cargo check` + a PTY/shell unit subset on macOS/Windows per PR; full oracles nightly on those runners | resolved |

---

## 3. Design

### 3.1 The `platform` module

One module per crate that needs it, or one shared `ocserve-platform` crate? **Decision:
no new crate.** A new crate would add a version to publish and a dependency edge for a
handful of functions. Instead:

- `ocserve-cli` gains `src/platform.rs` (paths, shell selection, free bytes, RSS of pid).
- `ocserve-pty` gains its own per-OS module beside `unix.rs`.
- `ocserve-tools` gets the shell + kill-tree seam from `ocserve-core::shell`.

Rationale: the repo's rule is to avoid schema/abstraction inflation. A `Platform` trait
would be an indirection paid on every call for three call sites.

### 3.2 Paths

`home()` precedence: `OCSERVE_HOME` → `HOME` → `USERPROFILE` → fail fast.
`data_root()`: `OCSERVE_DATA_DIR` → `~/.local/share/ocserve` (all platforms, on purpose).

**Why keep the XDG layout on Windows/macOS?** Because the data dir is an implementation
detail the user only sees when migrating, whereas *changing* it on upgrade silently
orphans an existing database. Upstream 1.18.31 computes
`XDG_DATA_HOME || ~/.local/share` on every platform too (verified in the frozen bundle:
`process.env.XDG_DATA_HOME||join($,".local/share")` with no `win32` branch for auth/data),
so the XDG layout is also the parity choice. `D-PLATFORM-XDG` records it.

### 3.3 Shell families

```
Shell::detect() -> Shell   // env OCSERVE_SHELL, config .shell, else platform default
Shell::Posix { path }      // /bin/sh, /bin/bash, git-bash on Windows
Shell::Cmd { path }        // %COMSPEC% or cmd.exe
Shell::Pwsh { path }       // pwsh/powershell
```
`Shell::command_string(&self, cmd: &str) -> Command` builds the argv. No quoting
translation. The frozen `bash` tool description is re-rendered per host (it already is a
captured artifact, so this is a data change, not a code change).

### 3.4 Process-tree containment

- Unix: existing `process_group(0)` + `killpg` (unchanged, proven).
- Windows: spawn into a Job Object created at process start with
  `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE` and per-job `ActiveProcessLimit`; timeout = terminate
  the job. Fallback if the spike fails: `taskkill /pid <pid> /T /F`.

### 3.5 PTY

| Platform | Implementation |
|---|---|
| Linux | existing `posix_openpt`/`ptsname_r`/`fork`/`TIOCSCTTY` (unchanged) |
| macOS | `openpty()` → `fork()` → child `login_tty(slave)` → `chdir` → `execve`; `TIOCSWINSZ` declared locally with a citation |
| Windows | ConPTY: `CreatePseudoConsole` + `STARTUPINFOEX` + two pipes |

`login_tty` closes the slave fd and calls `setsid` + `TIOCSCTTY` + `dup2`. Our Linux child
does the same steps by hand, so behaviour should match; that is a hypothesis until a macOS
runner proves create → type → resize → exit-code → kill.

### 3.6 Search engine

`ocserve-tools` and the `/find` route switch from spawning `grep` to the `regex` crate
(already in the binary via rolldown; no size cost). Deletes the silent substring fallback.
`/find` keeps its own two-pass shape (line pass + offset pass) so the wire shape is
unchanged.

---

## 4. Phases and gates

Every phase exits only with the gates in `TESTING.md §8` plus its own criteria.

### Phase S — platform seam (no Linux behaviour change)

Deliverables: `platform.rs` (paths, home, shell detect, free bytes); `EXE_SUFFIX` in
`which` and MCP spawn; `OOM_CHILD_WRAPPER` Linux-gated with an empty no-op elsewhere;
`blob.rs` directory-fsync made platform-correct; the two ungated unix imports fixed;
`partition`/`oom_score_adj`/cgroup paths report `Unavailable` off Linux; SIGTERM handling
(fixes a live Linux bug — journal shows zero `shutdown: draining`).

Exit:
- Linux: full workspace suite, clippy, guards, matrix, replay 26/0, pair 22/0 byte-identical
- `cargo check --target x86_64-pc-windows-msvc` green (whole workspace)
- `cargo check --target aarch64-apple-darwin` attempted; PTY expected-fail documented
- Guard rule 43 (platform API requires a platform guard)

### Phase G — search engine

Deliverables: `regex` in the grep tool and `/find`; remove the substring fallback; corpus
gate; `D-FIND-IGNORE` row.

Exit:
- A committed corpus at `bench/search/` (created by this phase) where every pattern's
  expected result is hand-verified; the run asserts the *new* semantics and records the old miss
- Perf: grep tool over a 1k-line file must drop from ~0.65 s to <10 ms (measured, not
  asserted — baseline recorded at 0.65 ms/line today)
- `/find` wire shape byte-identical for ASCII patterns that both engines interpret the same
- Phase-G guard (added in that phase, not yet implemented): no `grep`/`egrep` process
  spawn in production paths

### Phase P — macOS

**Verified on the macOS runner (2026-10-20):** `cargo test -p ocserve-pty` 7/7 including
`macos::tests::winsize_round_trips`, and `cargo test -p ocserve-http --test pty` 6/6. That
discharges `D-MACOS-RUNTIME`: the `forkpty` path spawns, writes, resizes (proving the Darwin
`TIOCSWINSZ` constant against `TIOCGWINSZ`), reports exit codes and kills. `platform-check
(macos-latest)` is now a required check alongside `platform-check (windows-latest)`.

Deliverables: PTY variant; path probe from the darwin freeze binary; RSS via `task_info`
(behind the existing `rss_bytes()` seam); trim measurement.

**Implemented.** Darwin's libc is missing `ptsname_r`, `TIOCSCTTY` and `TIOCSWINSZ` (verified in
the crate source), so the Linux open sequence cannot compile there — CI proved it with
`error[E0425]: cannot find function ptsname_r`. But Darwin *does* have `forkpty`, which performs
openpty + setsid + TIOCSCTTY + dup2 internally. So:

- `posix.rs` — post-fork behaviour (reader thread, waiter thread, write, group kill, exit-code
  mapping) in one copy used by both platforms.
- `unix.rs` — reduced to the Linux-specific open sequence (`posix_openpt` … `TIOCSCTTY`).
- `macos.rs` — `forkpty` + `execve`, with `TIOCSWINSZ`/`TIOCGWINSZ` declared as Darwin ABI
  constants (`0x80087467` / `0x40087468`) and a `TIOCGWINSZ` round-trip test that fails if the
  constant is wrong.

**Verification boundary.** `cargo check --target x86_64-apple-darwin` and
`--target aarch64-apple-darwin` under `-D warnings` both pass for the PTY modules. That is a
real Darwin compile check, run through a libc-only harness because the full crate needs a macOS
SDK for `libsqlite3-sys`. Runtime behaviour is **not** verified locally — the macOS CI runner is
the oracle (`cargo test -p ocserve-pty` + `--test pty` + the winsize round-trip). Until it
reports green, macOS runtime is `D-MACOS-RUNTIME`.

Exit: macOS runner green on the full suite *and* PTY e2e (create/type/resize/exit/kill),
`pair-check` against `opencode-darwin-arm64@1.18.31`, TUI attach. Any item not met is a
`D-MACOS-*` row, not a silent pass.

**Interim:** `ci.yml` has a `platform-check` job (macOS + Windows) that runs
`cargo check --workspace --all-targets`, plus the macOS-only guard suite, PTY
acceptance tests, and a `jsonschema` install (the event validator's dependency is
preinstalled on the Linux runner but absent from macOS's system Python; without
it rule 19 fails closed). It proves the type-level half — the two ungated
`std::os::unix` imports and any future `cfg` mistake — and nothing about behaviour. It runs
on the target OS because the C dependencies (SQLite, zstd, ring, blake3) cannot be
cross-compiled from Linux without the Windows SDK; that was verified here, not assumed
(`ml64.exe: No such file or directory`).

### Phase W — Windows

Deliverables: ConPTY (Job Object first); shell families; `scope_path` normalizer + corpus;
`EXE_SUFFIX` end-to-end (bun/node/MCP); `taskkill`/Job Object tree kill; long-path policy.

**Status: partially complete, verified in a real VM.** Windows 11 LTSC 24H2 under
`dockurr/windows` (KVM). What the VM proved, in the order it was found:

1. **The release binary did not start.** Exit `0xC0000135` (STATUS_DLL_NOT_FOUND): the MSVC build
   imports `VCRUNTIME140.dll`, which is on a developer machine and not on stock Windows. Fixed with
   `+crt-static` in `.cargo/config.toml` (verified by import table: dynamic = 1 VCRUNTIME import,
   static = 0; only the static binary runs).
2. **`config.username` was missing**, because the code reads `USER`/`LOGNAME` and Windows sets
   `USERNAME`. Real divergence from freeze on every Windows machine.
3. **`/pty/shells` returned `/bin/bash`, `/bin/zsh`, `/bin/sh`** — POSIX paths that do not exist.
   Now enumerates `pwsh` → `powershell.exe` → git-bash → `%COMSPEC%`, matching upstream's `win()`.
4. **`POST /shell` works**: the shell-family seam runs `cmd` and returns real output.
5. **pair-check inside the VM: 18 compared, 0 divergent** against `opencode-windows-x64@1.18.31`.

Still open (W1–W4): ConPTY (the PTY route still returns the documented stub error), Job Object
tree-kill, `scope_path` Windows normalizer, long-path policy. None of those are claimed.

Exit: Windows runner: full suite, PTY e2e, `cmd` and `pwsh` bash-tool e2e, `pair-check`
against `opencode-windows-x64@1.18.31`, WAL-on-network-share detection test.

### Phase V — verification and claims

Deliverables: per-platform pair/spec runs wired to nightly; README platform section;
`continue-on-error` removed; perf statement scoped per platform.

Exit: every claim in the README has a runnable artifact behind it on the platform it names.

---

## 5. Divergence ledger

| ID | Divergence | Status |
|---|---|---|
| `D-PLATFORM-XDG` | data/config paths stay XDG on macOS/Windows (parity with upstream's own resolution, and avoids orphaning existing data) | accepted |
| `D-PLATFORM-UNVERIFIED` | a platform shipped without a differential run against its own freeze binary | open until Phase V |
| `D-FIND-IGNORE` | `/find` uses an explicit exclude list, upstream uses gitignore discovery | open (predates this work) |
| `D-WIN-LONGPATH` | paths beyond `MAX_PATH` are rejected rather than transparently prefixed | open |
| `D-MACOS-TRIM` | `trim_heap()` is a no-op off glibc unless measurement justifies otherwise | open until measured |
| `D-WIN-WAL-NETWORK` | data directory on a network filesystem is refused at boot rather than degraded | accepted |

---

## 6. Performance position

Measured on Linux (the only platform with a harness): read-path capacity and latency as
recorded in `PERF-10X.md`. **No cross-platform performance claim is made or implied.**

Per platform, the acceptance is:
1. Allocation bytes and count per request, measured with the existing profiler, compared
   against that platform's own freeze binary.
2. p95/p50 of the read mix *only where a native harness exists*; the k6/Docker/cgroup rig
   is Linux-only and is not extended by pretending otherwise.
3. A named `D-` row for anything unmeasured.

Rationale: allocation churn is deterministic and portable (arXiv:2203.04394); wall-clock
across heterogeneous hosts is not (arXiv:2608.10956, 5% node-to-node variation).
