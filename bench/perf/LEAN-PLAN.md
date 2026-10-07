# Lean & Fast — full leverage sweep (branch `feat/perf-lean`)

Plan written to disk in plan mode, 2026-10-07. Zero product code changed while
writing it. One untracked file was created by mistake during a feasibility
probe: `crates/refine-store/examples/splice_probe.rs` (does not compile).
Kept as the Phase I probe; say the word and I delete it.

---

## 0. Two corrections to my own prior plan (read first)

**C1 — the micro-bench's "deep session" was empty.** `sqlite_tune_bench
--deep` auto-selects with `SELECT id FROM msg GROUP BY session_id ORDER BY
count(*) DESC LIMIT 1`. That returns a **message** id (`msg_fa92…`), so
`page_window`, `session_exists`, `session_wire`, `for_each_page` all
benchmarked a session with **0 messages**. Verified on the `.227` fixture:
`msgs in that session_id: 0`; the real biggest session is `ses_056d…` with
32,341 msgs. So those 6–19 µs figures are empty-result timings, and my claim
"parse+serialize = 62% of page cost" is **withdrawn** — the 19 µs vs 7 µs
delta was measuring nothing.

Consequence: **every store hot-path number from that bench is void until
re-run against a real session id.** `list_wire` (756–840 µs, 201 sessions)
and the two searches remain valid — they don't use `--deep`.

**C2 — k6's page numbers were real.** `bench/load` picks session ids from
`GET /session`, so E0's route attribution (page 2.54 ms CPU/req → F9 memo)
and every closed/arrival number stand. Only the store micro-bench was broken.

Fix shipped first (Phase I): the probe must assert `count(msg WHERE
session_id=deep) > 0` before printing, and `--deep` must query
`SELECT session_id …`.

---

## 1. The ceiling arithmetic (read before any lever)

Confirmation run `20261007T174649Z`, refine hot arm: 692,136 requests,
300.4 s CPU, p50 2.99 ms, p95 13.06 ms, 2 pinned physical cores (4 threads).

| quantity | value | source |
|---|---|---|
| CPU per request | **0.434 ms** | 300.4 s / 692,136 |
| server capacity at 2 cores | **~4,600 rps** | 2 / 0.434 ms |
| closed-model measured | 9,228 rps | k6 warm (S5 evidence: kernel buffers absorbed backpressure) |

Two hard ceilings follow, and they decide which levers are worth building:

1. **The k6 number is not the server's number.** 9,228 rps at 65% of 2 cores
   is a closed-model artifact, not capacity. Acceptance for this branch is
   therefore **CPU/req**, not rps — rps is reported but never gated.
2. **Refine's CPU/req (0.434 ms) is already 2× lower than freeze's recorded
   floor (≈0.95 ms/request).** Any *per-request* work removed here buys
   headroom, but the 10× headline cannot be re-earned from it.

Where the remaining 0.434 ms plausibly sits (to be measured in Phase I, not
assumed):

| layer | hypothesis | evidence status |
|---|---|---|
| JSON parse + merge + serialize of `data` blobs | real but **size unknown** | void (C1); 161.8 MB inline / 169,888 parts; 65% of parts <400 B |
| `list_wire` Value tree → serialize | **750–840 µs per list request** | measured, valid |
| SQLite read + FTS | page 7 µs / fts 101–112 µs / like 922 µs | valid (pre-DOM) |
| hyper/axum routing, body framing | unknown | never measured |
| spawn_blocking handoff, epoch/memo locks | unknown | never measured |

**Attribution is the deliverable of Phase I.** Building L1 (zero-parse splice)
before we know JSON is 10% or 60% of CPU is exactly the vibes-based spike I
rejected earlier for the memcache/allocator ideas.

---

## 2. Antagonism ledger (16 attacks, dispositions)

Fatal (do not build):

- **A1** — *L1 zero-parse splice is a wire-parity gamble.* serde_json's
  compact formatter emits `,` and `":"` with no whitespace
  (`ser.rs:1884-1893`, `begin_object_value`), so the DOM path **normalizes**
  formatting. 39% of stored inline samples contain `: ` / `, ` — i.e. those
  bytes came from upstream (not our serializer), so **pass-through would emit
  different bytes than today's path** for them. Any splice implementation must
  carry a per-part compactness pre-scan + DOM fallback, and must be proven
  byte-identical on the whole 169,888-row corpus, not a sample.
  **Disposition: L1 is gated behind Phase I measurement + corpus-wide byte
  proof.** If JSON <25% of CPU, L1 is dead and nothing is built.
- **A2** — *"upstream stores merged keys, so promote-to-payload is free."* It
  is not: 0% of 48,505 messages carry `id`/`sessionID` (measured), so the
  merge always inserts; and rewriting `info`/`inline` in place is a **schema
  v11 migration over 187 MB + byte-parity re-proof**. **Disposition: parked
  until L1 is proven valuable.**
- **A3** — *"F7 storage shrink = cache fit."* `part_search` text is a second
  copy of parts (378 MB of 1.98 GB). Dropping it shrinks the DB but SQLite's
  own page cache holds only the hot rows regardless of DB size, and
  `mmap_size=256 MB` measured **zero delta**. **Disposition: parked — no
  measured mechanism.**
- **A4** — *"jemalloc / mimalloc."* mimalloc already lost twice with
  evidence; jemalloc is a third guess at the same layer with no failure mode
  identified. **Disposition: parked.**
- **A5** — *C2 PGO re-trial* needs `llvm-tools-preview` and a **matched**
  `llvm-profdata` (system LLVM-22 was rejected earlier for version skew). Not
  available on `.227` or the host without install. **Disposition: conditional,
  gated on a toolchain check; not part of the core plan.**
- **A6** — *"more blocking threads."* `.227` has 4 cores, pool is 16,
  tokio workers 8 — already ≥ cores. Raising either measures nothing.
  **Disposition: rejected with the machine's core count as the reason.**
- **A7** — *"HTTP/2, compression, ETag, event coalescing."* All previously
  measured dead (k6 is h1 loopback; clients don't ask; 1,444 events/day).
  **Disposition: rejected, do not reopen.**

Serious (build, with the stated guard):

- **A8** — the profile is currently blind. `perf_event_paranoid=2`, no
  `perf`, no `valgrind` on `.227`. Bytehound works over any allocator and was
  decisive before. **Phase I must build the bytehound scenario or the whole
  sweep is guesswork.** Guard: no Phase II lever without a Phase I attribution
  row.
- **A9** — `for_each_message_json` holds **zero blob decode**; blob parts
  (8,253 rows / 380 MB) already stream via `from_slice`. The expensive shape
  is `msg.info` (25.7 MB, avg 528 B), which is one parse per *message*, not
  per part. **So L1's real target is `msg.info`, not `msg_part.inline`.**
  Guard: measure both separately before choosing.
- **A10** — `SEARCH_MEMO_CAP` is still **64** (the F9c raise to 2,048/64 MiB
  is in the *page/list* memos only). Search at 200-session spread hit an
  FTS walk for a miss. Possible real bug; low value. Probe only.
- **A11** — `list_directory`/`find_files` walk the filesystem **on a tokio
  worker** (no `spawn_blocking`). k6 measured `LIST_PATH=/tmp` (empty), so
  the benchmark **never touched this bug**. Real worktrees would stall a
  worker. Build as a correctness+isolation fix, not a speed lever.
- **A12** — `/api/agent` rebuilds and re-serializes a transformed Value tree
  per request; identical to the F5/F8 pattern that is already proven and
  epoch-guarded. Low risk.
- **A13** — `find_files` also runs inline on a worker and is the same class
  as A11.

Nitpicks handled in-flight: `r2d2`/`r2d2_sqlite` are declared and **unused**
(remove; dead dependency weight).

---

## 3. Phases

### Phase I — make it measurable (no product behaviour changes)

- **I1** `bench/perf/profile_page.sh`: build.rs + `bytehound` over
  `NM_PROF`/jemalloc-style hooks; scenario = 2,000 iterations of the real
  `for_each_message_json` page path against a **real** deep session
  (`ses_056d…`, 32,341 msgs), with `--deep` asserting
  `count(msg WHERE session_id=?) > 0`. Report top allocation sites with
  symbolized counts.
- **I2** Re-run `sqlite_tune_bench` with a **valid** deep session →
  establishes the real page/exists/wire baselines (C1 fix).
- **I3** Attribution table: for the page route, split CPU into
  (SQLite read) / (JSON parse) / (merge) / (serialize) / (HTTP+runtime) using
  the bytehound groups plus a `--json-off` splice spike run. **This table
  decides Phase II.**
- **I4** Cargo cleanup: drop unused `r2d2`, `r2d2_sqlite`.

### Phase II — build only what I3 implicates

Ordered by measured value, each with a keep-rule:

1. **L1** zero-parse assembly for the shape I3 names, with compactness
   pre-scan + DOM fallback, proven **byte-identical over all 169,888 inline
   parts and 48,505 infos** (a corpus differential test, not a sample),
   plus a fuzz equivalence property (`parse(splice(x)) == parse(dom(x))`).
   Keep if page CPU −20%+.
2. **M1** `load_sessions_wire_bytes`: build session JSON straight into a
   reused `Vec<u8>` with `to_writer` (no Value tree) — target the measured
   750–840 µs. Keep if list CPU −30%+.
3. **L4** constant `Bytes` bodies for the 8 shapes that serialize
   `json!({…})` per request (`/health`, `{}`-goldens, `/api/location`).
4. **L5** `/api/agent` (+ any other per-request transform) into the proven
   `Wires`/`Arc<Bytes>` epoch-guarded cache.
5. **A11/A13** move `list_directory` + `find_files` onto `spawn_blocking`,
   keep the existing bounds. Correctness first; report their real-path cost.

### Phase III — runtime/allocator, only if I3 attributes CPU to allocator

- jemalloc **behind an env switch** (never a hard swap) — keep only if it
  wins a CPU/req A/B at fixed rung *and* RSS.
- tokio worker count sweep (8 → 4 on this box) — keep only if measurable.
- `REFINE_TRIM` / `MALLOC_ARENA_MAX` re-verify — one-line changes, keep the
  measured winner.

### Phase IV — parked unless Phase II/III free real CPU

F7 storage shrink, X payload promote, F10 incremental projection, F7b
vocabulary change, `/file` dir memo. Each needs its own migration + rollback
+ measured win, and each is blocked on Phase I evidence existing.

### Phase V — verification

- A/B at G2 conditions (VU50, 4 threads, same box, interleaved).
- **Acceptance: CPU/req ≤ 0.39 ms (−10%) at equal offered load, zero
  errors, zero gate breaches, bytes identical to pre-branch where asserted.**
  Throughput reported, never gated (C1 ceiling).
- Guard: never accept a change that reduces rps below 9,000 in the closed
  model *unless* CPU/req improved — because of C1 those can disagree.
- Full gates each phase: fmt, `clippy --workspace --all-targets -D warnings`,
  workspace tests, `check-guards.sh`, `check-matrix.sh`, `replay-check.sh`
  (26/0), bytehound scenario.
- `STORAGE.md` §1.3 records the whole ledger incl. **rejected** levers.

---

## 4. Honest risk statement

- Every JSON-path lever trades **implementation simplicity** for CPU. Today
  the code is "parse, merge, serialize" — obviously correct. A splice is a
  byte-level parser with a fallback. If Phase I says JSON is <25% of CPU, the
  right move is **not to build L1 at all**, and I will say so.
- The 10× headline is already banked and is not at risk from this branch;
  nothing here can be honestly claimed as a new headline number unless CPU/req
  moves ≥10%.
- Two plan-mode discipline notes: (a) I wrote one file by mistake
  (`examples/splice_probe.rs`, untracked, non-compiling); (b) the k6 closed
  number's queueing artifact means the next A/B must **interleave** and
  report CPU/req from `cpu s`, not rps.

## 5. Open questions for the user

1. **Branch scope** — ship Phase I–III only (code paths + runtime, no
   migrations), or green-light Phase IV's migration cycle (F7/X) if Phase I
   implicates storage?
2. **The stray file** — keep `splice_probe.rs` as the Phase I probe (it needs
   40 lines fixed) or delete it now?
3. **Acceptance metric** — I propose CPU/req (−10%) as the gate and rps as
   informational, because the current 9,228 rps is a queueing artifact. Do
   you want the stricter "rps must not fall" clause kept as well?