# Phase 2 / L2 (PRE-REGISTERED — not built): prompt-flood concurrency

Status: design only (2026-10-06). L1 measures reads with zero provider
traffic; L2 is where the deterministic dummy upstream (parity stub) enters,
and ONLY as a controlled constant.

## Goal

Answer the concurrent-session question for the **write path**: how many
simultaneous prompts do the servers accept, where does per-session
serialization bind, and when does the documented flood behavior fire.

## Design

1. Boot the parity stub on the host (`parity-stub` image, port 8194);
   fixture `baseURL` rewritten to `127.0.0.1:8194` for both arms
   (identical files → identical behavior).
2. k6 scenario `constant-arrival-rate`: `POST /session/{sid}/prompt_async`
   spread across the seeded session pool at `LOAD_PROMPT_RPS`, plus a
   hot mode concentrating on one session.
3. Measured: 204-acceptance latency (server-side, NOT model time —
   `STUB_TOK_PER_SEC` high so completion is out of the critical path),
   achieved-vs-offered acceptance, error taxonomy (429/503/409/lock),
   memory slope per arm.
4. Target facts to confirm/refute: per-session prompt lock serializes
   turns (same-session arrival > 1 in flight must queue or 409),
   cross-session scales until CPU/memory, refine's 64-lock flood → 503
   behavior under `prompt_locks` cap.

## Kill criteria (pre-registered)

- **K-L2a**: stub pacing dominates acceptance latency (>50% of p95 is
  wait-for-stub, not server) even at high `STUB_TOK_PER_SEC` → measure
  acceptance before enqueue only, else abort the phase.
- **K-L2b**: freeze lacks the prompt route shape refine needs (404/schema
  drift on the async path) → scope to routes both serve; record as D-row.
- **K-L2c**: fixture sessions can't hold locks (state shape mismatch,
  cf. D-PAIR-3) → fix the fixture first; no measuring on misaligned data.

## Non-goals

Model quality, multi-turn flows, tool loops (owned by calculator/pipeline
benchmarks), SSE fan-out (S3/S5), scheduling changes (suite stays MANUAL).
