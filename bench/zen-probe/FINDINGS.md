# Zen FreeTier investigation — FINDINGS

**Verdict: DISCRIMINATOR FOUND.** Keyless `opencode/big-pickle` completions from a
non-opencode client require exactly two things:

1. **`x-opencode-session` must be opencode's native session-id format** —
   `ses_` + 12 hex chars + 14 base62 chars (26 chars after the prefix).
   A well-formed-but-different id (e.g. `ses_` + 64 hex) → `403 FreeTierError`.
   The gate does **not** verify the session exists (fresh purely-random correct-format
   ids pass; replayed ids pass).
2. **The request body must contain a `tools` array** (opencode's tool definitions;
   `tool_choice:"auto"` rode along in the passing shape). Tiny body without tools → 403,
   even with correct ids, `stream:true`, or real headers.

Everything else tested was irrelevant: TLS fingerprint (JA3 byte-identical between the
bun-compiled ELF and bun-compiled... and bun 1.3.14 fetch — both
`d871d02cecbde59abbf8f4806134addf` + ALPN http/1.1), HTTP version (1.1 both), header
names/casing/order (byte-set identical after MITM diff; `Accept` position differs and does
not matter — passing runs used fetch's order), User-Agent composite, `x-opencode-client`
value, runtime (node and bun both pass), Authorization (`Bearer public`, 13 chars — no
minted token exists anywhere; zero auth/account calls precede the completion), body size
(62 KB real body and ~1 KB minimal body both pass *when tools are present*), `stream`
flags, `max_tokens`, session/message freshness.

## Pre-registered result (criteria written before runs)

- **PASS recipe:** E13-class request — native-format session id + body with `tools`
  (+ real header set). **3/3** `200` with fresh ids each run.
- **Negative controls (proven red):** E9 (64-hex session, tools body) → 403;
  E10 (native session, no tools) → 403; E12 (+ stream flags, no tools) → 403;
  original replica (native session id `ses_ef6907c85ffe6TokiMy6mi1FG3` but no tools,
  plain UA, `client=tui`, 24-char msg id) → 403.
- Rate cap honored: ≈25 LLM-endpoint POSTs across the whole investigation (cap 30).

## Reproduction

`bench/zen-probe/replica-v2.js` (`E13` stage), executed inside the container by
`./host.sh rv2 E13`. Header set captured verbatim from the real client via MITM
(`real-headers.json`), body = minimal + real `tools` array. Two independent passes:
`E7` (full 62 KB real body) and `E13` (minimal body + tools).

## What is proven about their client (captured, request #1 of r2 flows)

```
POST https://opencode.ai/zen/v1/chat/completions        HTTP/1.1
Authorization: Bearer public                            (len 13)
User-Agent: opencode/1.18.31 ai-sdk/provider-utils/4.0.23 runtime/bun/1.3.14
x-opencode-client: cli
x-opencode-project: global
x-opencode-request: <native msg id>
x-opencode-session: <native ses id>
Accept: */*   Accept-Encoding: gzip, deflate, br, zstd   Connection: keep-alive
body: {model, max_tokens:32000, stream:true, stream_options:{include_usage:true},
       tool_choice:"auto", messages:[system,system,user], tools:[28 defs]}
→ 200 SSE (data: {"id":"07129a36…","model":"big-pickle",…})
```

Provider endpoints (models cache): `opencode` → `https://opencode.ai/zen/v1`,
`opencode-go` → `https://opencode.ai/zen/go/v1`. The catalog's `auth/api` base never
appears in the client binary. `x-opencode-ticket` is PTY-only (not an LLM header).

## Host-pcap evidence (2026-10-05 01:30:28–01:37:28 IST, preserved before shred)

New ClientHellos to `opencode.ai` during the window (t = seconds after start):

| t (s) | src port | JA3 | ALPN | note |
|---|---|---|---|---|
| 107.13 | 46346 | `2d4d79b97f1a73de252bea30fc451db6` | http/1.1 | same-second triple |
| 107.58 | 46354 | `2d4d79b97f1a73de252bea30fc451db6` | http/1.1 | |
| 108.09 | 53186 | `2d4d79b97f1a73de252bea30fc451db6` | http/1.1 | |
| 200.71 | 32970 | `bcf60a9758a9ca1ec3cf59b70c43fe28` | http/1.1 | lone different JA3 |
| 269.89 | 34422 | `2d4d79b97f1a73de252bea30fc451db6` | http/1.1 | |
| 375.80 | 40038 | `2d4d79b97f1a73de252bea30fc451db6` | http/1.1 | |

(`2d4d…`/`bcf60a…` are node-undici probes of the earlier session; attribution was
ambiguous — per-scenario container pcaps replaced it.) Their 01:30:53 probe completion
reused a pre-existing TLS connection, so the host pcap never held their plaintext; the
in-container MITM did.

## Other established facts

- Brew opencode 1.18.31 = 185,030,784 B ELF, needs only `GLIBC ≤ 2.17`, **zero
  `SSLKEYLOGFILE` strings** (bun-compiled → keylog capture of their client is impossible;
  that was the failed approach of 01:14–01:41).
- The ELF honors `https_proxy` (R1: 35 CONNECT dials incl. `opencode.ai:443`) and trusts
  `SSL_CERT_FILE`/`NODE_EXTRA_CA_CERTS` (R2: full completion through local-CA MITM).
- On-disk credential state: `credential`/`account_state`/`control_account` tables empty;
  `account` holds only a chatgpt.com login; no zen/opencode token anywhere.
- No runtime token minting: the r2 flow inventory (82 flows) shows only npm metadata,
  oauth2.googleapis (codex-auth plugin), github raw (codex plugin), context7 MCP, and the
  single zen call.

## Ladder results

| Rung | Result |
|---|---|
| R1 logging CONNECT proxy | PROXY_HONORED (35 dials) |
| R2 MITM + local CA | **plaintext captured** — completion through MITM = 200 |
| R3 JA3 s0 vs s1 | different (ELF `d871d02c…` vs node `0cce74b0…`) — later falsified as causal by E2/E4 |
| R4 binary forensics | zen bases, header set, UA template recovered |
| R5a replica/bun/E1–E5 | all 403 (with bad session id — the confound the bisect exposed) |
| R5b E6/E7 (ids fixed, real body) | **200** |
| R5c E8/E9 (field attribution) | E8 session-format ok → 200; E9 session-bad → 403 ⇒ **session format gated** |
| R5d E10/E12/E13 (body attribution) | no-tools → 403 (stream flags don't help); +tools → **200** ⇒ **tools gated** |

## Free-model sweep (follow-up — E13 recipe, model swapped, fresh ids)

Catalog: `opencode` provider holds 116 models — 36 zero-cost ("free"), 80 paid.
Swept 9 free + 1 paid control (one request each):

| model | status | reading |
|---|---|---|
| `mimo-v2.6-flash-free` | **200** | second model completes keyless with the same recipe |
| `fledge-alpha-free` | 403 `FreeTierError: not available in your country` | **cleared** the within-OpenCode gate → geo wall (yet their own client completed fledge at 00:17 IST the same day — geo verdict is not purely IP) |
| `deepseek-v4-flash-free` | 400 `Upstream request failed: Model is unavailable` | **cleared** the gate → upstream dead |
| `glm-5-free`, `kimi-k2.5-free`, `minimax-m3-free`, `qwen3.6-plus-free`, `ling-3.0-flash-free`, `grok-code` | 401 `ModelError: not supported` | rejected before our gate — likely stale catalog vs backend's live set |
| `claude-3-5-haiku` (paid control) | 401 `not supported` | no keyless credit path, as expected |

Conclusion: the discriminator (session-id format + `tools` in body) is **not
big-pickle-specific** — it gates the whole within-OpenCode check; per-model
availability then layers on top (geo, upstream, model whitelist). Two of nine
free models actually complete; two more pass the gate and fail elsewhere.

## Not bisected (honest residuals)

- `tools` vs `tool_choice` (both present in E13; one could be the actual check).
- UA composite / `x-opencode-client: cli` / header `Accept` position were never
  individually A/B'd — the recipe simply uses the real client's values.
- Whether an empty `tools: []` passes (E13 used the real 28-tool array).

## Safety / method notes

- All experiments ran in disposable containers (`bench/zen-probe/`); host services were
  read-only throughout (AGENTS §2 rule 11, added by this investigation's Phase 0).
- Wire artifacts (pcaps, flows, MITM dumps) shredded at teardown; authorization values
  never enter committed files.
- User decision: **report only** — no refine port in this pass.
