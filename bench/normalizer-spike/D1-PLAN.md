# D1 execution plan — rolldown normalizer linked into refine (2026-10-06)

Decision record: D1 (bundle it, raise ceiling) — STRESS-RESULTS.md §Phase 3
dispositions; user confirmation after ELI5 + size-context analysis (20.3 MB
still 9× smaller than upstream's 185 MB binary; gate = self-imposed tripwire
that did its job). This plan is the executable spec; attack ledger A1–A15 +
final-pass additions live in the session record and are dispositioned here.

## Design (final-pass corrections applied)

- **Insertion point: `Sidecar::load_raw`** — single choke point covering boot
  AND respawn replay (`ensure_alive` calls `load_raw` directly, lib.rs:337).
  `loads` keeps storing the **RAW** entry → replay re-normalizes (warm ≈ hash
  cost; wipe-while-running self-heals at respawn AND next boot).
- **No spawn signature change**: `Sidecar.normalize_root: Option<PathBuf>` +
  `set_normalize_root()` (main.rs boot sets `data_dir/normalized`); **None →
  skip** → every existing test/parity driver keeps raw behavior untouched.
  `ensure_alive` copies the field onto the fresh sidecar before `*self = fresh`.
- **Conditional emit (scanner)**: rolldown's `OutputChunk.imports`/`dynamic_imports`
  provably MISS non-analyzable dynamic imports (rolldown src `impl_visit.rs:241`:
  "No import record - either @vite-ignore or non-static dynamic import") —
  the scanner reads **emitted code text**:
  - parse `from "x"` / `import("x")` / `import \`x\`` / `require("x")` / side-effect `import "x"`;
    resolve simple const-bound identifiers via one text-level const map
    (`const BUN_SPEC = "bun:sqlite"` → its value);
  - relative (`./ ../ / file:`), `node:`, `bun:` prefixes, node builtin list
    (incl. subpaths like `fs/promises`) → **clean**;
  - bare package or UNRESOLVABLE argument (identifier w/o binding, template
    with `${}`) → **needs-ancestry** (conservative direction: false positive
    costs tidiness, false negative impossible by construction);
  - clean → `<root>/<path-hash>/<stem>.normalized.mjs`; needs-ancestry →
    beside entry (only occurs inside opencode's cache today);
  - on data-dir emit, **best-effort remove** stale beside-entry normalized
    pair (auto-heals spike leftovers incl. reliary8's two files); symmetric
    when flipping the other way.
- **Cache**: `sha256(entry bytes + embedded K5b shim + salt)`; warm gate checks
  BOTH candidate locations (hash match ∧ output exists ∧ size>0). Salt =
  `NORMALIZER_VERSION` const (bump on bundler-behavior change).
- **Atomicity**: tmp+rename with pid-unique tmp name; hash file after rename
  (tmp also renamed); stale `*.tmp` older than 1 h removed best-effort on build.
  **Never** direct `fs::write` to a final normalized path → new guard rule 8.
- **CPU**: whole normalize in `spawn_blocking` (A2 bug class); rolldown on a
  current-thread runtime inside (spike pattern).
- **Kill switch** `REFINE_PLUGIN_NORMALIZE=0` → `Skipped` (rolldown never
  executed → panic residual gated) — A3.
- **Metric**: `refine_metrics::labeled_counter("refine_plugin_normalize_total",
  &format!("result=\"{}\"", r), 1)` with r ∈ warm|built|error|disabled|passthrough.
- **A2 log** on `built`/`error` only: destination, ms, RSS before→after.
- **Suffix guard**: entry ending `.normalized.mjs` → passthrough, no rebuild.
- **Rejected**: load-retry on error-string matching (3 runtime dialects) →
  named residual: future module-not-found load failure ⇒ scanner rule or retry.
- **Deps**: rolldown, rolldown_common, sha2, refine-metrics added to
  refine-plugin (no cycles; cargo check proves).

## Sequence (each gated)

1. Plan doc (this file) → shim copy `host/shim-dual-sqlite.mjs` (K5b verbatim)
2. `normalize.rs`: scanner + bundler + outcome enum `Warm|Built{..}|Skipped|
   Fallback{..}|Passthrough` — **scanner controls first (red→green)**:
   real magic-context output ⇒ needs-ancestry (would catch the metadata bug),
   real reliary8 ⇒ clean, real codex-auth ⇒ **needs-ancestry** (evidence: `await import(specifier)` = "proper-lockfile", its dep — first "clean" assumption was wrong, scanner right; beside-entry = cache, no repo writes), synthetic computed-pkg
   import ⇒ needs-ancestry, node:/relative fixture ⇒ clean (cache tests behind
   the M4b existence-gate idiom; env mutation behind a shared test lock —
   guard rule 7 forbids process-global counter asserts in src, env is the
   same hazard class → tests serialize on a parking_lot mutex)
3. Wire `load_raw` (metric+log+RPC with normalized path, `loads` keeps raw) +
   `ensure_alive` field copy + main.rs `set_normalize_root` + behavior tests
   (warm / stale-rebuild / error→raw fallback / kill-switch / wipe→rebuild
   incl. sidecar-replay variant runtime-gated / suffix passthrough) — **each
   with planted control proven red→green**
4. fmt + clippy -D + workspace tests → release build → **measure final binary**
   → set nightly.sh ceiling = ceil(measured, MiB) with provenance comment
   (date, measured, D1 rolldown link, decision record)
5. `cargo audit` + `cargo deny` — every new-crate finding dispositioned by name
6. Full gates: tests, guards (rule 8 + planted negative), matrix, replay 26/0
7. **Live battery**: cold boot (3× built + RSS lines) → restart (3× warm) →
   hook sets 8/9/1 → kill-switch boot (disabled, no rolldown) → wipe+restart
   (rebuild) → metric present in /metrics → reliary8 leftovers auto-removed
8. Docs: TRACEABILITY `K-PLUGIN-NORM`, SRE knob+metric, FINDINGS "D1 executed"
   + residuals, STRESS addendum + repro note → one feat commit → deploy →
   fresh 24 h soak (resets clock — accepted)

## Acceptance (inherited pre-registration)

STRESS M1–M3/S1–S4 (green) · scanner controls 5/5 · behavior tests 6/6 with
planted controls · gates green · live battery 7/7 · ceiling set from measured
final number · residuals R1 (panic→abort), R2 (cache writes = same trust
boundary), R3 (+0.6 s cold rebuild after plugin update) documented not hidden.
