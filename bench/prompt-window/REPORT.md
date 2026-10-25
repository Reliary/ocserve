# Bounded prompt load: equivalence and timing

Measured 2026-10-24 on the live read-only database
`$HOME/.local/share/ocserve/ocserve.db`, branch `feat/bounded-prompt-load`.
Reproduce with:

```sh
cargo test -p ocserve-core --no-run
BIN=$(ls -t target/debug/deps/ocserve_core-* | grep -v '\.d$' | head -1)
OCSERVE_CORPUS_DB="$HOME/.local/share/ocserve/ocserve.db" \
OCSERVE_CORPUS_SESSION=<session id> \
  "$BIN" bounded_prompt_load_matches_real_corpus --ignored --nocapture
```

The probe is **interleaved A/B/A/B with the first pair discarded as warm-up**,
takes the best of three, and asserts the emitted `ChatMessage` lists are
byte-identical between the full load and the bounded window (also with a
transform listener forced on).

## Results

| session | messages | stored parts | full load (best of 3) | bounded (best of 3) | mode |
|---|---|---|---|---|---|
| `ses_fb13…` (deep) | 16,616 | 85.0 MB | 4.63 s / 5.25 s (two runs) | 1.45 s / 1.59 s | `window_budget` |
| `ses_001a…` (compacted) | 99 (compacted) | small | 117 ms | 22 ms | `window_compaction` |
| `ses_fb59…` (dense) | 4,307 | dense | 2.89 s | 2.56 s | `window_budget` |

Emitted messages: byte-identical in all cases (`serde_json` comparison).

A later re-run on a busier host measured 8.63 s full / 2.46 s bounded on the
deep session. Absolute times move with host load; the interleaved ratio is the
claim, and it held in every run. No latency claim is made beyond these
single-box observations.

## Honest limits

- The third row is the limit case: when the 8 MB budget covers most of the
  stored bytes, the window saves little. The win scales with how much of a
  session is older than the budget.
- These are single-box debug-build numbers on an idle machine. No claim is made
  about release-build latency, concurrent load, or memory; the corpus probe
  measures wall time only.
- `mode` is read from `ocserve_prompt_history_load_total`; `full` means the
  window could not be proven and the pre-change path ran.

## Negative controls proven during development

- A window starting after the compaction tail fails the equivalence assertion
  (`negative_control_shrunken_window_diverges`).
- Keying the compaction boundary on the newest summary `seq` instead of the
  newest anchor `seq` reds
  `newest_anchor_wins_even_when_a_late_summary_lands_on_an_older_anchor`
  (restored green).
- Returning the anchor instead of the tail reds
  `compaction_window_starts_at_tail`.
