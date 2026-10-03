# sift-at-boundary gate — results (2026-10-03)

**Verdict: FAIL** (1/4 criteria unmet) → per pre-registration, `REFINE_SIFT`
default stays **OFF**. Results committed as-is (README §Gate).

| criterion | required | measured | result |
|---|---|---|---|
| engagement | ≥2/3 of gate runs compress ≥1 output | **4/8** (50%) | ❌ |
| cost (weighted, input+4×output) | gate ≤0.85 × baseline | 1,115,153 vs 2,710,360 = **−58.9%** | ✅ |
| score median non-regression | gate ≥ baseline | 1.0 vs 1.0 | ✅ |
| no panics/restarts | zero | zero (`died=false` everywhere) | ✅ |

18 runs (3 tasks × 3 seeds × 2 conditions) interleaved, first-mover alternating;
2 pre-registered infra retries (t2-s2-gate timed out twice at 600 s → cell of
record missing; t2-s3-gate attempt 1 timed out, attempt 2 succeeded). Model
`opencode-go/mimo-v2.6-flash` both conditions, one fresh refine process per run.

## Paired WC ratios (gate / baseline)

| pair | ratio | pair | ratio |
|---|---|---|---|
| t1 s1 | **0.54** | t2 s1 | **0.02** |
| t1 s2 | 0.99 | t2 s3 | **0.17** |
| t1 s3 | 0.99 | t3 s1 | 2.46 |
| | | t3 s2 | 1.87 |
| | | t3 s3 | 2.45 |

Score: **1.0 on every completed run, both conditions** — zero score
regression anywhere (t2 checks: exact facts; t1:5/5 pristine-failing tests
fixed in all runs; t3: all runs exited 0 with correct counts).

## Why engagement failed (two distinct mechanisms, forensically confirmed)

1. **Models self-limit output — the dominant cause (t1-s2, t1-s3).**
   The model piped its own test runs (`| grep -E "^(FAIL|ERROR):"`,
   `| tail -15/60`) so every bash output stayed under the 4 KiB threshold
   (max observed 3,312 B). `refine_sift_*` never fired (bytes_in=0). The
   agent behaving well is what starves the feature — not a sift defect.
2. **High-entropy unique-line output does not shrink (t3-s2, t3-s3).**
   `app.py`'s `[load NNNNN] … checksum=N` lines are all distinct; sift's
   `compress_unified` returned the input unchanged (verified offline:
   raw=51,200 → sifted=51,200). Metrics correctly recorded `raw=1`
   fallback (bytes_in=bytes_out, no inflation). Sift engaged but had
   nothing to compress — the no-inflation invariant did its job.

## Cost-pass caveat (read the driver, not just the total)

The −58.9% aggregate is driven overwhelmingly by **t2** (ratios 0.02/0.17):
baseline t2 runs churned for 369–518 s re-deriving facts from a 110 KB
report (WC 563k–920k), while gate runs compressed that history and converged
in 145–576 s. t1 pairs are ~neutral (self-piped outputs, nothing to
compress). **t3 gate pairs all cost 1.9–2.5× baseline** — byte accounting
shows no inflation (bytes_out ≤ bytes_in on every run); the divergence is
behavioral: gate runs issued more turns/commands than their baselines. With
n=3 per cell this is recorded as unattributed behavior variance, not as a
sift cost regression — and not hidden either.

## Signal preservation (the actual risk the gate exists to test)

- Real failing-test output (8.9 KB): all failure/assertion lines survived
  sift (4/4 matched) — offline probe, recorded in P1c commit.
- t2 gate runs answered from compressed output: score 1.0 while carrying
  only **164 bytes** of the 51,200-byte report (bytes_out) — the summary
  line the scorer needs survived a 99.7% reduction.
- No run scored below its baseline counterpart; median non-regression held.

## What this does NOT establish

- Not a general ≥15% WC claim (driver is one task; paired ratios spread
  0.02–2.46).
- Not evidence that sift harms t3-style tasks (behavior variance, n=3,
  can't separate arm effect from turn-count divergence).
- Engagement on "well-behaved model" workloads may stay ~50% regardless of
  threshold tuning — models that pipe/redirect will always dodge the ≥4 KiB
  surface.

## Data

- `results/runs.jsonl` — all 18 rows + both retry attempts (pre-registration:
  no exclusions; failed attempts kept visible)
- `results/verdict.json` — criteria evaluation + paired ratios
- runner: `run_gate.py`, fixtures frozen under `fixtures/`
