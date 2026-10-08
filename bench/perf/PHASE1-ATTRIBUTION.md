# Phase I — CPU/alloc attribution of the read path (I1/I2/I3)

Branch `feat/perf-lean`, 2026-10-07. This is the deliverable that decides
Phase II. Everything here is measured, not estimated.

## Method

- Profiler: **bytehound 0.11.0**, built from source (`koute/bytehound`,
  `preload` cdylib + CLI; `CFLAGS=-std=gnu11` for the GCC-15/C23 default,
  `yarn` on PATH for the WebUI build). **LD_PRELOAD over glibc** on the
  `.227` runner against the **debug** binary (release is `strip=true`, so no
  symbols) — the same method as `bench/profiling/REPORT.md`.
- Scenario: `crates/ocserve-store/examples/page_prof.rs`, 20 warmup + **400
  measured iterations** of the real production mix against the real fixture:
  page of 50 messages (`for_each_message_json` full assembly), session-list
  wire bytes, `session_exists`, and one search. Non-empty deep session is
  **asserted** (the C1 bug class).
- Attribution: bytehound's REST API (`/data/{id}/allocation_groups`) grouped
  by backtrace, then each frame resolved through `nm -n` on the PIE with the
  load base derived from `_start`, walking outward to the first frame that is
  `ocserve_*` / `serde_json` / `rusqlite` / `sqlite3`. gimli/`alloc`/`std`/
  libc/bytehound frames are plumbing and are skipped.

## I2 — corrected store baseline (real 32,341-message session, 109,083 parts)

| query | before (empty session — WITHDRAWN) | corrected |
|---|---:|---:|
| `page_messages` (SQL window) | 6 µs | **26 µs** |
| `session_exists` | 2 µs | 2 µs |
| `load_session_wire` | 8 µs | 4 µs |
| `for_each_page` (SQL + full JSON) | 19 µs | **1,249 µs** |
| `load_sessions_wire_bytes` (201 sessions) | 756 µs | 684 µs |
| `search_parts` fts (`"the"`) | 109 µs | 105 µs |
| `search_parts` LIKE (`"zebra"`) | 943 µs | 943 µs |

The ratio that matters: **JSON assembly is 48× the SQL select** (1,249 vs
26 µs) for a 50-message page. The old numbers hid this completely.

## I3 — allocation attribution (4,758,136 allocations, 1,867 MB, 400 iters)

~11,900 allocations and 4.67 MB per iteration. By count:

| site | allocs | % | bytes | % |
|---|---:|---:|---:|---:|
| `serde_json` NonNull\<str\>::len (Value string allocs) | 2,214k | **46.5%** | 255 MB | 13.7% |
| `serde_json` `ValueVisitor::visit_map` | 1,032k | 21.7% | 317 MB | 17.0% |
| `for_each_message_json` (output chunk build) | 402k | 8.4% | **679 MB** | **36.4%** |
| `merge_columns` | 351k | 7.4% | 22 MB | 1.2% |
| `sqlite3EndBenignMalloc` | 155k | 3.3% | 92 MB | 4.9% |
| `serde_json` parse_str_bytes | 130k | 2.7% | 123 MB | 6.6% |
| `serde_json` Map::insert | 116k | 2.4% | 55 MB | 2.9% |
| `SearchHit::clone` | 105k | 2.2% | 70 MB | 3.8% |
| `BlobStore::get` | 6k | 0.1% | 123 MB | 6.6% |
| `page_messages` / `load_sessions_wire` / `session_exists` | ~50k | 0.1% | ~13 MB | 0.7% |

**Verdict: JSON DOM work is ~86% of allocations by count and ~75% by bytes.
SQLite + page assembly + search are noise.** Phase II therefore builds
exactly one thing: **eliminate the parse→merge→serialize round trip.**

## I3 — L1/A1 re-litigation: the zero-parse splice is now justified, with a
fatal caveat found by measurement

serde_json's compact formatter emits `,` and `":"` with **no whitespace**
(`serde_json-1.0.150/src/ser.rs:1884-1893`), i.e. the DOM path *normalizes*
formatting. 39% of stored inline samples contain `: `/`, ` (upstream's
Bun-written spacing), so **raw passthrough is not byte-identical** to today's
output for those rows. The splice must therefore:

1. copy member bytes verbatim, but
2. reproduce serde's compact separators when re-emitting a value, and
3. fall back to the DOM path on anything it does not prove.

The **corpus differential** (all 169,888 inline parts + 48,505 infos, not a
sample) is the gate. If the splice cannot be proven byte-identical on that
corpus, it does not ship.

Secondary finding: `for_each_message_json` allocates **679 MB / 400 iters =
1.7 MB per page** building output chunks, versus 26 µs of SQL. Even a
perfect splice leaves the *output* buffer cost, so the page path also needs
a reused buffer (the F5/F9 wire-buffer pattern) — L1 and L1b are one change.

## Rejected by this profile

- **I4 Cargo cleanup** (unused `r2d2`/`r2d2_sqlite`): dead weight, no perf
  effect. Do it for hygiene, not speed.
- **Phase III allocator/compiler work**: allocation *count* is not the
  bottleneck — 99.99% of allocations are temporary and freed in the same
  iteration (1,867 MB moved across 400 iterations, RSS flat). A different
  allocator (jemalloc/mimalloc) changes the *cost per malloc*, not the
  4.76M mallocs. **Parked unless Phase II leaves malloc as the top cost.**
- **mmap_size / page_size**: already A/B'd to zero delta
  (`STORAGE.md §1.2`).
- **PGO (C2)**: `llvm-tools-preview`/`llvm-profdata` not present; needs a
  toolchain install. Parked, not blocked.

## The one lever that matters

```
page route CPU  ≈  JSON parse (46%+21%)  +  merge (7%)  +  re-serialize (13%)
                  ≈  86% of allocations,  1,249µs of 1,275µs wall
```

Phase II = replace parse→merge→serialize with a **verified byte-identical
zero-copy splice + reused output buffer**, gated on a corpus-wide byte proof
and a k6 CPU/req A/B.