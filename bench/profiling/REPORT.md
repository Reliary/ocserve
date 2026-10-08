# Phase-0 memory profile (2026-10-04)

**Method:** bytehound (built from source: submodule init, yarn via
`npm -g --prefix ~/.local`, `CFLAGS=-std=gnu11` for its old mimalloc vs
GCC15/C23) over **LD_PRELOAD on glibc** against the **debug** binary (release
is `strip=true` → no symbols). Isolated fake `HOME` (fake provider, no
plugins), one session seeded with 500 msgs × ~2 KB, 40 s mixed load:
436 prompt_async + 67 full `/message` fetches, 0 errors, RSS ≤ 263 MB.
Artifacts: `scenario.py` (re-runnable), `top-alloc.rhai` (analysis),
raw `.dat` kept out of repo (214 MB).

## Headline numbers (debug build, ~70 s process lifetime)

| Metric | Value |
|---|---|
| Total allocations | **4,483,992** (~64k/s under load) |
| Temporary (churn) | **4,441,202 — 99.5%** |
| Live-at-exit | 42,790 (count; includes legit caches) |

## Top allocation groups by size (symbolized)

1. **`pcache1Alloc` → `for_each_message_json` (`ocserve_http::get_messages`,
   lib.rs:1041)** — SQLite page-cache pages allocated through glibc on the
   **fresh reader connection per `/message` fetch** (thread-per-fetch,
   `std::thread::spawn` frames visible in the same group). Biggest *bytes*.
2. **`serde_json::Value::to_string` inside `for_each_message_json`
   (lib.rs:728)** — per-message JSON serialization: fresh `String` per frame,
   ~70k allocs in 40 s — the second-biggest group and the purest churn.
3. Remaining top groups: prompt-path serde/regenerated bodies + thread-spawn
   scaffolding — same families (serialization + per-request connection/thread
   setup), no mystery consumer.

## Decisions this evidence supports

1. **Allocator: adopt glibc honestly; attack churn first.** The dominant
   costs are *allocation count* (serialization buffers, per-fetch SQLite
   pcache, thread spawn), which a malloc swap does not remove. mimalloc
   wiring stays a *retention* option only if the post-fix soak still shows
   step-function RSS retention (morning soak showed +76/+58 MB steps before
   the drain fix — unproven after it).
2. **Highest-leverage code fixes (phase 1):**
   - serialize `/message` frames + prompt request bodies into **reused
     buffers** (`to_writer` into a per-thread `Vec<u8>` instead of
     `Value::to_string`) — kills group #2 outright;
   - **reader-connection reuse** (thread-local) — kills group #1's
     per-fetch pcache allocation (this is now an *allocation* argument, not
     a latency guess);
   - `/message` **fetch dedup/etag remains parked**; thread-per-fetch stays
     bounded by the streaming design.
3. **Region/arena (ISMM'26): demoted again** — buffer reuse achieves the
   same bulk-free effect for the dominant objects with far less machinery.
4. `sort_by_size` group order + `only_leaked` section are in `top-alloc.rhai`
   for re-runs against future changes.
