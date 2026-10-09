# Web UI performance + scaling — pre-registered plan

Status: **complete** (2026-10-09). Driver: the web UI is the primary
surface for the hosted app and a common complaint against opencode is that
it is slow; the user also observed that **many sessions / large git diffs**
slow upstream down. This program fixes both, with the honesty rule that no
number ships unless a committed, repeatable measurement produced it.

Delivered: W1 compression (boot wire 9.10 MB → 1.03 MB br), W2 validators
(ETag/304, immutable assets), W3 lazy byte-bounded asset cache (0 at boot),
W4 session-list scaling (freeze ListQuery parity), W5 route closure + `/vcs/*`,
and **P6 embedded version-matched UI** (embedded-first, proxy fallback;
`bench/webui/app/1.18.31.pack.zst`). Guards: rule 16 (spec coverage) + rule 17
(embedded-UI skew) supersede the old rule 15.

## Baseline (measured 2026-10-08, this box, ocserve :4912 vs freeze :4901)

Boot bytes on wire (browser, cold then reload — identical because nothing
is cached):

| Resource | Bytes |
|---|---:|
| `/provider` (JSON) | 6,265,216 |
| `index-*.js` | 2,808,653 |
| `Inter.ttf` | 874,708 |
| `index-*.css` | 482,489 |
| small API + HTML | ~12,000 |
| **first load total** | **~10,200,000** |
| **reload total** | **~10,200,000** (no cache headers anywhere) |

In-page `/provider` cost: **205 ms download + 157 ms JSON.parse** (parse is
the app's own; we do not shrink it without a shape break).

Compression potential (measured, one-time cost):

| Payload | raw | gzip-6 | brotli-q5 | zstd-3 |
|---|---:|---:|---:|---:|
| `/provider` | 6,265,216 | 350,659 | 195,384 | 356,920 |
| `index-*.js` | 2,808,653 | 836,226 | 665,836 | 837,469 |
| CSS | 482,489 | 70,276 | 63,770 | — |
| `Inter.ttf` | 874,708 | 458,029 | 414,682 | — |

CPU: gzip-6 on the 6.2 MB provider = 0.12 s; br-q11 = 20 s (rejected);
br-q5 = 0.18 s (precomputed once per write-epoch, off the request path).

Scaling cliffs (the user's report, mechanism-verified):

- **Many sessions**: freeze `/session` defaults to limit **100** of 1,267
  rows; the web UI sidebar refetches with a growing limit; the v2 list path
  pulls 5,000 (2,002,539 B, 0.61 s). ocserve today **ignores all list query
  params** and returns every session every call — a parity break (`roots`
  filtering) and a scaling hole.
- **Large git diffs**: freeze's `vcs.diff` requests patches with
  `PATCH_CONTEXT_LINES = 2^31 − 1` (full file context) and synthesizes
  per-file untracked patches. Cost is payload + client render, not process
  count (corrected: freeze batches tracked patches in one `git patchAll`;
  the earlier "3 subprocesses per file" claim was wrong and is withdrawn).

Route-surface hole (same crash class as the 2026-10-08 `/pty/shells` bug):
the **web bundle** calls `/global/config`, `/vcs/status`, `/vcs/diff`,
`/api/health`; all four currently fall through ocserve's catch-all to the
HTML proxy. They degrade silently today (the app swallows the error) but are
wrong. The old guard (rule 14, SDK list) missed them because it is
**method-blind** and read the SDK, not the bundle.

## Changes

- **W0** Pre-register (this file); extract the bundle route surface to
  `bench/webui-routes.txt` (+ `bench/webui/extract-routes.py`); baseline
  script `bench/webui/baseline.sh`.
- **W1 Compression.** Wire-route variants (provider/config/agent/command/
  config_providers/console/capabilities) precomputed once per write-epoch
  and memoized as Bytes alongside the F5 cache — gzip-fast + brotli-q5.
  Dynamic JSON > 1 KiB compressed on the fly (gzip-fast). Assets br/gzip
  once then cached. SSE (`text/event-stream`) never compressed. Every
  compressible response carries `Vary: Accept-Encoding`. zstd dropped:
  precomputed brotli wins on bytes and the 20 ms zstd dependency buys
  nothing on the client-visible axis.
- **W2 Validators.** ETag (content hash) on wire routes, session list,
  search, and vcs.diff; `If-None-Match` → 304. Content-hashed assets get
  `Cache-Control: public, max-age=31536000, immutable` + ETag. HTML passes
  the upstream `no-store` through unchanged.
- **W3 Proxy.** Stream non-HTML bodies instead of `resp.bytes()`; lazy
  byte-accounted LRU asset cache (**cap 32 MiB, allocated 0 at boot, grows
  only on first proxy fetch** — MEMORY.md row); metrics
  `ocserve_webui_asset_cache_bytes/_hits/_misses`. Optional
  `OCSERVE_UI_PREWARM=1` (default **off**) prewarms the shell assets.
- **W4 Session list scaling + parity.** Freeze `ListQuery` semantics:
  `limit` (default 100), `roots`, `search` (title LIKE), `start`, `path`,
  `directory`, `scope`; SQL pushdown; param-keyed bounded memo (16).
- **W5 Route closure + VCS.** `/global/config` (freeze shape),
  `/file/status` → `[]`, `/find/symbol` → `[]`; `/vcs/status`, `/vcs/diff`
  (`mode=git|branch`, `context` passthrough, 10 MB total-patch cap parity),
  `/vcs/diff/raw`. `vcs.apply` stays out (0 call sites, write path — PLAN
  §17 citation).
- **W6 Guard (superseded 2026-10-09).** Rule 15's Cloudflare-latest
  `bench/webui-routes.txt` is replaced by rule 16 (spec-driven coverage over
  the frozen contract, exact-citation) + rule 17 (the embedded pinned UI's
  routes must be a subset of the frozen spec — `bench/webui/check-app-skew.py`).
  This tracks the *version-matched* bundle rather than CF's newest.

## Targets / kill criteria (measured before claiming)

1. Boot wire bytes ≤ 1.5 MB (from ~10.2 MB); reload ≤ 50 KB.
2. `/provider` with `Accept-Encoding: br` ≤ 300 KB; refetch with
   `If-None-Match` → 304.
3. **Identity byte-parity**: a client sending no `Accept-Encoding` / no
   `If-None-Match` sees byte-identical responses to today (replay 26/0, pair,
   k6 GATED unchanged). Verified before merge.
4. SSE bytes unchanged (never compressed).
5. Identity-path CPU/req unchanged within noise (revert the change if +2 ms
   p95 on the interleaved A/B).
6. VCS diff honours `context`/`mode`; ≤ freeze's subprocess shape; 10 MB cap.
7. Web-asset cache: 0 bytes at boot with no UI traffic; ≤ 32 MiB; eviction
   proven by a bound test.

## Out of scope (with reasons)

- Shrinking the upstream JS bundle / `Inter.ttf` → app.opencode.ai (not
  ours; content-hashed, now immutable-cached).
- Injecting preload/prefetch hints into proxied HTML → mutates upstream
  response; rejected (fragile, parity break).
- `/api/*` v2 surface, `/sync/*`, workspace/worktree groups → v2-only or
  0-call-site; PLAN §17 citations.
- In-memory caching of `/provider` JSON keyed only by mtime → the write-epoch
  memo already covers it exactly.

## Citations

RFC 9111 (HTTP caching / revalidation); RFC 9110 §8.8 (ETag/If-None-Match),
§12.5.3 (Vary); RFC 7932 (Brotli); Wang, "A survey of web caching schemes for
the Internet", SIGCOMM CCR 1999 (hierarchical caching, validators);
Dean & Barroso, "The Tail at Scale", CACM 2013 (per-request tail latency
compounds on the client-visible path); simdjson arXiv:1902.08318 (parse cost
is often the hidden cost — recorded honestly as app-owned here); h2c
rejected: browsers require TLS/ALPN (RFC 7301, RFC 9113 §3.2).
