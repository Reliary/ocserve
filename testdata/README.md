# testdata — provenance and privacy rules

All fixtures here are **contract artifacts** captured from a real upstream
opencode 1.18.31 server (the frozen wire contract), or synthetic values shaped
like upstream's. None of them contain user session content: session ids are
placeholders, titles are probe labels, and the recorded streams are trivial
hello-world exchanges. The privacy rules below are enforced by the pre-commit
hook (`.githooks/pre-commit`).

## What is here

| Path | Content | Recreatable? |
|---|---|---|
| `golden/manifest.json` | 31 GET routes: key-path projections + byte/status expectations, captured from upstream 1.18.31 | Yes — `ocserve replay --record` against any 1.18.31 server (see below) |
| `golden/*.body` | Byte-exact response bodies for `bytes`-mode routes (health, 404 envelope, busy status, vcs) | Same recording flow; bodies are environment-normalized at record time |
| `golden/message_page_contract.json`, `summarize_contract.json` | Structural notes for paging/compaction wire shapes | Same |
| `m2/llm_stream_*.bin` | Recorded OpenAI-compatible SSE streams — now living in `crates/ocserve-llm/testdata/` so the crate packages independently | Not bit-identical (provider output varies); the *shape* is what tests assert |
| `m2/session_fixture.json`, `prompt_response.json`, `tool_fixture.json` | Prompt-contract captures: message/part shapes as upstream emits them | Yes — re-capture via a prompt against upstream |

## Routes not carried in the corpus

`/tui/*` (external-controller ingress) is event-shaped: every endpoint
answers `true` and publishes onto the global SSE stream. Its contract is
asserted in `crates/ocserve-http/tests/tui.rs` against semantics probed live
from freeze 1.18.31 (2026-10-08), including the upstream quirks (open-themes
publishes `session.list`; unknown execute-command publishes empty
properties; toast duration defaults 5000).

## Privacy rules (binding)

1. **No real session ids.** Every `ses_*` in this tree is either a placeholder
   (`ses_000000000000AAAAAAAAAAAAAA`, `ses_000000000000BBBBBBBBBBBBBB`) or the
   synthetic `ses_nonexistent` used by the 404 golden. A real id appearing here
   is a bug.
2. **No personal content.** No prompts, titles, paths, or emails from any
   operator's sessions. The pre-commit hook scans this directory (the old
   `testdata/` email exemption was removed in the 2026-10-08 hygiene pass —
   an all-history scan proved no fixture ever needed it).
3. **Corpus updates are reviewed diffs.** Per `AGENTS.md` §2.7 golden corpus
   changes land in their own commit with justification, never silently
   regenerated.

## Re-recording

The corpus is the oracle for `scripts/replay-check.sh` and CI. To re-record
against a different upstream version (contract bumps — see `PLAN.md` §8):

```sh
# against a local upstream server:
ocserve replay --record --target http://127.0.0.1:4901 --out testdata/golden
# then review the diff, run ./scripts/replay-check.sh, and commit as test(corpus):
```

Re-recording is a contract decision, not a test fix: if a route's bytes changed
between upstream versions, that is a freeze-bump event and everything the
matrix says about the affected requirement must be re-verified.
