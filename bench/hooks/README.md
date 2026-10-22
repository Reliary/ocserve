# Hook-contract corpus

Answers one question mechanically: **does ocserve fire the same plugin hooks, with
the same payloads, in the same order, and does it USE what the plugin returns?**

## Why a capture and not the type declarations

`@opencode-ai/plugin` declares the hook surface, but a `.d.ts` cannot tell you:

- which hooks the reference *actually fires* (several are declared and never called);
- which optional keys are present in a real payload (`undefined` disappears in JSON);
- the order, or the frequency (per turn vs per LLM request);
- whether a returned value is consumed.

The reference implementation is the oracle. `scripts/capture-hooks.sh` boots it under a
fixture `HOME` with a **probe plugin** loaded from an absolute path and records every
trigger with its real `input`/`output`.

## Pieces

| file | role |
|---|---|
| `probe.mjs` | passive: records, never mutates |
| `probe-active.mjs` | mutates every output the documented way, so CONSUMPTION is observable |
| `toolstub.py` | deterministic provider that emits exactly one tool call per turn |
| `normalise.py` | `surface` builds the comparable shape; `diff` reports differences |
| `order_check.py` | asserts the per-request order `system -> params -> headers` |
| `effects.py` | asserts an active-probe mutation reached its boundary |
| `freeze.surface.json` | the reference surface (regenerate, never hand-edit) |
| `allow.txt` | named divergences only; each line cites a `D-HOOK-*` row |

## What "identical" means

- **Key sets per hook, per direction.** A key present on one side only is a difference.
- **Value KIND.** `model: "stub-1"` vs `model: {providerID, modelID}` is a difference even
  though the key sets match — that is the class that disabled Magic Context.
- **Trigger presence.** A hook the reference fires and we do not is a failure, not a
  warning.
- **Order**, collapsed to one token per LLM request for the per-request group, so a turn
  with a different number of model calls does not read as a reordering.
- **Effects.** Three boundaries: the provider request body, the persisted tool part, and
  the shell child's environment.

`null == absent` is the only normalisation that is not about ids/times/paths: the
reference passes `undefined` (dropped by JSON) where we pass `null`, and no plugin can
observe the difference.

## Known divergence

`D-HOOK-TRANSFORM-PER-TURN` — `experimental.chat.messages.transform` runs once per turn
here, once per LLM request upstream. Encoded in `normalise.ORDER_IGNORED`, with the row in
`PLAN.md §16.9`.

## Usage

```sh
./scripts/hook-contract-check.sh            # capture both arms, diff, order, effects
./scripts/hook-contract-check.sh --self-test
HOOKS_PROBE=active ./scripts/capture-hooks.sh   # one arm, with the mutating probe
```

Needs the vendored freeze (`bench/parity/vendor/opencode`); CI does not have it, so this is
a nightly/local gate. Guard rule 47 is the CI-side proxy: it runs the selftest and checks
that the gate still wires capture + diff + effects.

## Bumping the pin

1. `./scripts/capture-hooks.sh` (reference arm).
2. `OCSERVE_HOOKS_BIN=target/release/ocserve ./scripts/capture-hooks.sh`.
3. `python3 bench/hooks/normalise.py surface …` for both, then `diff`.
4. Every new difference is either a bug in ocserve or a new `D-HOOK-*` row. Never add a
   bare allowlist line.
