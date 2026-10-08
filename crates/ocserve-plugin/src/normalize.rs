//! Plugin entry normalization (D1 — bench/normalizer-spike/STRESS-RESULTS.md
//! §Phase 3 dispositions; executable spec: bench/normalizer-spike/D1-PLAN.md).
//!
//! One self-contained ESM output per plugin entry, loadable with zero
//! runtime-specific patches on bun/node/deno (spike K1–K7), cached by
//! content hash, emitted either beside the entry (bundle still has
//! runtime-resolved bare specifiers → needs node_modules ancestry — the
//! K4 case) or under ocserve's own `normalized/` tree (fully self-contained
//! → zero writes into user repos; D1-PLAN scanner rule).
//!
//! Every failure degrades to the raw entry (loud in the log, metric
//! `error`), never worse than today's behavior. `OCSERVE_PLUGIN_NORMALIZE=0`
//! skips before rolldown is ever constructed (the panic=abort residual,
//! A3). Callers: `Sidecar::load_raw` (boot AND respawn replay).

use anyhow::{Context, Result, bail};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// Bump when bundling/alias/scanner behavior changes → all caches rebuild.
const NORMALIZER_VERSION: &str = "normalize-v1";
/// K5b dual-runtime sqlite shim, inlined into every bundle as a virtual
/// module (bun:sqlite → native on bun, computed node:sqlite elsewhere —
/// spike K5b; static node:sqlite would fail bun's graph link).
const SHIM_SRC: &str = include_str!("host/shim-dual-sqlite.mjs");
/// Crash-orphaned tmp files older than this are removed best-effort.
const STALE_TMP_SECS: u64 = 3600;

/// Result of one normalize attempt. The returned `PathBuf` (tuple) is what
/// the caller must load; the raw entry stays the replay/canonical source.
#[derive(Debug)]
pub enum Outcome {
    /// Content hash matched an existing complete output.
    Warm(PathBuf),
    /// Freshly bundled + written. `beside` = destination class (D1-PLAN).
    Built {
        path: PathBuf,
        ms: u128,
        beside: bool,
        rss_before_kb: i64,
        rss_after_kb: i64,
    },
    /// Kill switch or no normalize root configured → raw.
    Skipped(PathBuf),
    /// Normalize failed for a real reason → caller loads raw (A: never
    /// worse than today). `path` is always the raw entry.
    Fallback { path: PathBuf, err: String },
    /// Entry already is a normalized output (double-normalize guard, A11).
    Passthrough(PathBuf),
}

impl Outcome {
    /// Metric label for `ocserve_plugin_normalize_total{result=…}`.
    pub fn label(&self) -> &'static str {
        match self {
            Outcome::Warm(_) => "warm",
            Outcome::Built { .. } => "built",
            Outcome::Skipped(_) => "disabled",
            Outcome::Fallback { .. } => "error",
            Outcome::Passthrough(_) => "passthrough",
        }
    }
}

// ---------------------------------------------------------------------------
// Scanner (D1-PLAN): classifies EMITTED code text. rolldown's
// OutputChunk.imports/dynamic_imports provably MISS non-analyzable dynamic
// imports (rolldown src/ast_scanner/impl_visit.rs:241 "No import record -
// either @vite-ignore or non-static dynamic import"), so metadata alone
// would misclassify magic-context's computed transformers import as clean →
// broken plugin. Unknown parse forms err TOWARD needs-ancestry (false
// positives cost tidiness only; false negatives are impossible by
// construction).
// ---------------------------------------------------------------------------

/// `true` = the bundle still resolves something at runtime against its own
/// location → must be emitted beside the entry (node_modules ancestry).
pub fn needs_ancestry(code: &str) -> bool {
    let consts = const_map(code);
    for spec in specifiers(code, &consts) {
        match spec {
            // unresolvable argument (bare identifier / template with ${}) →
            // conservative: unknown ⇒ needs ancestry
            None => return true,
            Some(s) if !clean_spec(&s) => return true,
            Some(_) => {}
        }
    }
    false
}

/// Runtime-resolved import specifiers: `Some(parsed)`, `None` = could not
/// determine statically. Word-boundary correct (a keyword token must not be
/// glued to identifier chars on either side — a naive offset check here
/// would self-defeat and classify every bundle as needs-ancestry).
fn specifiers(code: &str, consts: &HashMap<String, String>) -> Vec<Option<String>> {
    let mut out = Vec::new();
    let b = code.as_bytes();
    let n = b.len();
    let idc = |c: u8| c.is_ascii_alphanumeric() || c == b'_' || c == b'$';
    let boundary = |start: usize, len: usize| -> bool {
        (start == 0 || !idc(b[start - 1])) && b.get(start + len).map(|&c| !idc(c)).unwrap_or(true)
    };
    let is_quote = |c: u8| c == b'"' || c == b'\'' || c == b'`';

    for (kw, len) in [("import", 6usize), ("require", 7usize), ("from", 4usize)] {
        let mut i = 0;
        while let Some(rel) = code[i..].find(kw) {
            let start = i + rel;
            i = start + len;
            if !boundary(start, len) {
                continue;
            }
            let mut j = start + len;
            while j < n && b[j].is_ascii_whitespace() {
                j += 1;
            }
            if j >= n {
                continue;
            }
            if b[j] == b'(' {
                // dynamic import( / require( — expression position: an
                // unresolvable argument is unknown → conservative (None).
                // `from` is NOT call syntax (`Array.from(x)` is a method).
                if kw != "from" {
                    j += 1;
                    while j < n && b[j].is_ascii_whitespace() {
                        j += 1;
                    }
                    out.push(parse_arg(code, j, consts));
                }
            } else if is_quote(b[j]) {
                // Static specifier position (`from "…"`, side-effect import):
                // ESM specifiers cannot contain raw newlines or `${}`, so a
                // parse failure means this `from`/`import` token is string
                // CONTENT in source text, not an import statement → skip.
                // (Pushing None here would false-dirty every clean bundle
                // that mentions `from "` inside an error message.)
                let after_dot = start > 0 && b[start - 1] == b'.';
                if ((kw == "from" && !after_dot) || kw == "import")
                    && let Some(sp) = parse_arg(code, j, consts)
                {
                    out.push(Some(sp));
                }
                // bare `require` without call parens is not an import
            }
        }
    }
    out
}

/// Parse an import argument starting at `j` (the quote / backtick / ident).
fn parse_arg(code: &str, j: usize, consts: &HashMap<String, String>) -> Option<String> {
    let b = code.as_bytes();
    match b.get(j)? {
        q @ (b'"' | b'\'') => read_quoted(code, j + 1, *q).map(|(s, _)| s),
        b'`' => {
            let (s, end) = read_quoted(code, j + 1, b'`')?;
            if s.contains("${") {
                None // template with substitution → unknown → conservative
            } else {
                let _ = end;
                Some(s)
            }
        }
        c if c.is_ascii_alphabetic() || *c == b'_' || *c == b'$' => {
            // identifier argument → one-level const resolution
            let mut k = j;
            while k < b.len() && (b[k].is_ascii_alphanumeric() || b[k] == b'_' || b[k] == b'$') {
                k += 1;
            }
            consts.get(&code[j..k]).cloned()
        }
        _ => None, // computed expression → unknown → conservative
    }
}

/// Read a quoted literal from `start` (after the opening quote) up to the
/// next unescaped matching quote. Returns (value, index_after_close).
fn read_quoted(code: &str, start: usize, quote: u8) -> Option<(String, usize)> {
    let b = code.as_bytes();
    let mut k = start;
    let mut val = String::new();
    while k < b.len() {
        match b[k] {
            c if c == quote => return Some((val, k + 1)),
            b'\\' => {
                k += 1;
                if k < b.len() {
                    val.push(b[k] as char);
                    k += 1;
                }
            }
            c => {
                val.push(c as char);
                k += 1;
            }
        }
    }
    None // unterminated → unknown
}

/// One-level constant table: `const NAME = "lit"` / backtick-literal-without-`${`.
/// Word-boundary correct (`constx` is not a declaration).
fn const_map(code: &str) -> HashMap<String, String> {
    let mut m = HashMap::new();
    let b = code.as_bytes();
    let n = b.len();
    let idc = |c: u8| c.is_ascii_alphanumeric() || c == b'_' || c == b'$';
    for kw in ["const", "let", "var"] {
        let len = kw.len();
        let mut i = 0;
        while let Some(rel) = code[i..].find(kw) {
            let at = i + rel;
            i = at + len;
            let before_ok = at == 0 || !idc(b[at - 1]);
            let after_ok = b.get(at + len).map(|&c| !idc(c)).unwrap_or(true);
            if !before_ok || !after_ok {
                continue;
            }
            let mut k = at + len;
            while k < n && b[k].is_ascii_whitespace() {
                k += 1;
            }
            let name_start = k;
            while k < n && idc(b[k]) {
                k += 1;
            }
            if k == name_start {
                continue;
            }
            let name = code[name_start..k].to_string();
            let mut ws = k;
            while ws < n && b[ws].is_ascii_whitespace() {
                ws += 1;
            }
            if ws >= n || b[ws] != b'=' {
                continue;
            }
            ws += 1;
            while ws < n && b[ws].is_ascii_whitespace() {
                ws += 1;
            }
            if ws >= n {
                continue;
            }
            let q = b[ws];
            if (q == b'"' || q == b'\'')
                && let Some((v, _)) = read_quoted(code, ws + 1, q)
            {
                m.entry(name).or_insert(v);
            } else if q == b'`'
                && let Some((v, _)) = read_quoted(code, ws + 1, b'`')
                && !v.contains("${")
            {
                m.entry(name).or_insert(v);
            }
        }
    }
    m
}

/// A specifier that resolves WITHOUT the entry's node_modules ancestry.
fn clean_spec(s: &str) -> bool {
    if s.is_empty() {
        return true;
    }
    if s.starts_with("./") || s.starts_with("../") || s.starts_with('/') {
        return true;
    }
    if s.starts_with("file:") || s.starts_with("data:") {
        return true;
    }
    if s.starts_with("node:") || s.starts_with("bun:") {
        return true;
    }
    let first = s.split('/').next().unwrap_or("");
    is_node_builtin(first)
}

/// Node.js builtin module names (first path segment). Missing an exotic one
/// errs toward needs-ancestry (conservative — only affects destination).
fn is_node_builtin(first: &str) -> bool {
    matches!(
        first,
        "assert"
            | "async_hooks"
            | "buffer"
            | "child_process"
            | "cluster"
            | "console"
            | "constants"
            | "crypto"
            | "dgram"
            | "diagnostics_channel"
            | "dns"
            | "events"
            | "fs"
            | "http"
            | "http2"
            | "https"
            | "inspector"
            | "module"
            | "net"
            | "os"
            | "path"
            | "perf_hooks"
            | "process"
            | "punycode"
            | "querystring"
            | "readline"
            | "stream"
            | "string_decoder"
            | "sys"
            | "timers"
            | "tls"
            | "tty"
            | "url"
            | "util"
            | "v8"
            | "vm"
            | "wasi"
            | "worker_threads"
            | "zlib"
    )
}

// ---------------------------------------------------------------------------
// Bundling (spike K1 — one chunk, treeshake off, bun:sqlite alias w/ guard)
// ---------------------------------------------------------------------------

/// Bundle one entry to a single ESM chunk. Errors are diagnostics, never
/// panics (spike S4: syntax/cycle/garbage/huge all → Err).
fn bundle(entry: &Path) -> Result<String> {
    use rolldown::plugin::{
        HookLoadArgs, HookLoadOutput, HookResolveIdArgs, HookResolveIdOutput, HookUsage, Plugin,
        PluginContext, SharedLoadPluginContext,
    };
    use rolldown::{
        Bundler, BundlerOptions, InputItem, LogLevel, OutputFormat, Platform, TreeshakeOptions,
    };
    use std::borrow::Cow;
    use std::future::Future;

    const SHIM_ID: &str = "ocserve:bun-sqlite-shim";

    #[derive(Debug)]
    struct AliasBunSqlite {
        shim_src: &'static str,
    }

    impl Plugin for AliasBunSqlite {
        fn name(&self) -> Cow<'static, str> {
            Cow::Borrowed("ocserve-bun-sqlite-alias")
        }

        fn resolve_id(
            &self,
            _ctx: &PluginContext,
            args: &HookResolveIdArgs<'_>,
        ) -> impl Future<Output = rolldown::plugin::HookResolveIdReturn> + Send {
            let hit = args.specifier == "bun:sqlite";
            // Guard: the shim's own `import("bun:"+"sqlite")` may be
            // constant-folded back to a literal — it must stay a RUNTIME
            // import (catchable off-bun), never aliased to itself.
            let from_shim = args.importer == Some(SHIM_ID);
            async move {
                if hit && from_shim {
                    Ok(Some(HookResolveIdOutput {
                        id: "bun:sqlite".into(),
                        external: Some(rolldown_common::ResolvedExternal::Bool(true)),
                        ..Default::default()
                    }))
                } else if hit {
                    Ok(Some(HookResolveIdOutput::from_id(SHIM_ID)))
                } else {
                    Ok(None)
                }
            }
        }

        fn load(
            &self,
            _ctx: SharedLoadPluginContext,
            args: &HookLoadArgs<'_>,
        ) -> impl Future<Output = rolldown::plugin::HookLoadReturn> + Send {
            let hit = args.id == SHIM_ID;
            let code = self.shim_src;
            async move {
                if hit {
                    Ok(Some(HookLoadOutput {
                        code: code.into(),
                        map: None,
                        side_effects: None,
                        module_type: Some(rolldown_common::ModuleType::Js),
                    }))
                } else {
                    Ok(None)
                }
            }
        }

        fn register_hook_usage(&self) -> HookUsage {
            HookUsage::ResolveId | HookUsage::Load
        }
    }

    let opts = BundlerOptions {
        input: Some(vec![InputItem {
            name: Some("plugin".to_string()),
            import: entry.to_string_lossy().into_owned(),
        }]),
        cwd: entry.parent().map(Path::to_path_buf),
        format: Some(OutputFormat::Esm),
        platform: Some(Platform::Node),
        // D1-PLAN: side effects ARE the plugin registration contract.
        treeshake: TreeshakeOptions::Boolean(false),
        log_level: Some(LogLevel::Warn),
        ..Default::default()
    };
    let mut bundler = Bundler::with_plugins(
        opts,
        vec![rolldown::plugin::__inner::Pluginable::new_shared(
            AliasBunSqlite { shim_src: SHIM_SRC },
        )],
    )
    .map_err(|e| anyhow::anyhow!("bundler init: {e:?}"))?;

    // CPU-bound work runs on our own current-thread runtime; the caller
    // (load_raw) has already moved us onto spawn_blocking (A2 class).
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .context("normalize runtime")?;
    let out = rt
        .block_on(bundler.generate())
        .map_err(|e| anyhow::anyhow!("{e:?}"))?;
    for w in &out.warnings {
        tracing::warn!("normalize bundler warning: {w:?}");
    }
    let mut chunks = Vec::new();
    for a in &out.assets {
        if let rolldown_common::Output::Chunk(c) = a {
            chunks.push(c);
        }
    }
    if chunks.len() != 1 {
        bail!(
            "expected exactly 1 chunk, got {} ({:?})",
            chunks.len(),
            chunks
                .iter()
                .map(|c| c.filename.as_str())
                .collect::<Vec<_>>()
        );
    }
    Ok(chunks[0].code.clone())
}

// ---------------------------------------------------------------------------
// Cache + atomic writes
// ---------------------------------------------------------------------------

fn content_hash(entry: &Path) -> Result<String> {
    let mut h = Sha256::new();
    let bytes = std::fs::read(entry).with_context(|| format!("read {}", entry.display()))?;
    h.update(bytes);
    h.update([0xff]);
    h.update(SHIM_SRC.as_bytes());
    h.update(NORMALIZER_VERSION.as_bytes());
    Ok(format!("{:x}", h.finalize()))
}

/// Data-dir subdirectory for an entry: path-hash (16 hex chars).
fn path_key(entry: &Path) -> String {
    let mut h = Sha256::new();
    h.update(entry.to_string_lossy().as_bytes());
    format!("{:x}", h.finalize())[..16].to_string()
}

fn warm(out: &Path, want: &str) -> bool {
    let hash_path = hash_path_of(out);
    let Ok(h) = std::fs::read_to_string(&hash_path) else {
        return false;
    };
    h.trim() == want && std::fs::metadata(out).map(|m| m.len() > 0).unwrap_or(false)
}

fn hash_path_of(out: &Path) -> PathBuf {
    let mut s = out.as_os_str().to_os_string();
    s.push(".hash");
    PathBuf::from(s)
}

/// Write data via pid-unique hidden tmp + rename (STORAGE crash discipline).
/// Guard rule 8 bans direct `fs::write` to final normalized paths.
fn atomic_write(dest: &Path, data: &str) -> Result<()> {
    let dir = dest.parent().context("dest has no parent")?;
    std::fs::create_dir_all(dir).with_context(|| format!("mkdir {}", dir.display()))?;
    let name = dest
        .file_name()
        .context("dest has no name")?
        .to_string_lossy()
        .into_owned();
    let tmp = dir.join(format!(".{name}.{}.tmp", std::process::id()));
    std::fs::write(&tmp, data).with_context(|| format!("write {}", tmp.display()))?;
    std::fs::rename(&tmp, dest).with_context(|| format!("rename {}", dest.display()))?;
    Ok(())
}

/// Remove crash-orphaned `.{name}.*.tmp` older than STALE_TMP_SECS.
fn sweep_stale_tmp(dest: &Path) {
    let (Some(dir), Some(name)) = (dest.parent(), dest.file_name()) else {
        return;
    };
    let prefix = format!(".{}.", name.to_string_lossy());
    let Ok(rd) = std::fs::read_dir(dir) else {
        return;
    };
    for e in rd.flatten() {
        let fname = e.file_name().to_string_lossy().into_owned();
        if fname.starts_with(&prefix) && fname.ends_with(".tmp") {
            let stale = e
                .metadata()
                .ok()
                .and_then(|m| m.modified().ok())
                .and_then(|t| t.elapsed().ok())
                .map(|age| age.as_secs() > STALE_TMP_SECS)
                .unwrap_or(false);
            if stale {
                let _ = std::fs::remove_file(e.path());
            }
        }
    }
}

fn rss_kb() -> i64 {
    std::fs::read_to_string("/proc/self/status")
        .ok()
        .and_then(|s| {
            s.lines()
                .find(|l| l.starts_with("VmRSS:"))
                .and_then(|l| l.split_whitespace().nth(1))
                .and_then(|v| v.parse().ok())
        })
        .unwrap_or(-1)
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

/// Normalize `entry` for loading. Returns (path-to-load, outcome). On any
/// failure the raw entry is returned with `Fallback` — the load proceeds
/// exactly as it does today (never worse than raw; D1-PLAN).
pub fn normalize_or_raw_sync(root: Option<&Path>, entry: &Path) -> (PathBuf, Outcome) {
    let raw = entry.to_path_buf();
    // A11: never re-normalize an output.
    if entry
        .file_name()
        .map(|n| n.to_string_lossy().ends_with(".normalized.mjs"))
        .unwrap_or(false)
    {
        return (raw.clone(), Outcome::Passthrough(raw));
    }
    let Some(root) = root else {
        // No root configured (tests, sidecars without it) → today's behavior.
        return (raw.clone(), Outcome::Skipped(raw));
    };
    // A3: kill switch checked BEFORE rolldown exists anywhere.
    if matches!(
        std::env::var("OCSERVE_PLUGIN_NORMALIZE").as_deref(),
        Ok("0")
    ) {
        return (raw.clone(), Outcome::Skipped(raw));
    }

    let work = match entry.canonicalize() {
        Ok(w) => w,
        Err(e) => {
            return (
                raw.clone(),
                Outcome::Fallback {
                    path: raw,
                    err: format!("canonicalize {}: {e}", entry.display()),
                },
            );
        }
    };
    let hash = match content_hash(&work) {
        Ok(h) => h,
        Err(e) => {
            return (
                raw.clone(),
                Outcome::Fallback {
                    path: raw,
                    err: format!("{e:#}"),
                },
            );
        }
    };

    let stem = work
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "index".into());
    let out_name = format!("{stem}.normalized.mjs");
    let beside = work.with_file_name(&out_name);
    let data_dir = root.join(path_key(&work));
    let data_out = data_dir.join(&out_name);
    let hash_file = root.join(path_key(&work)).join(format!("{out_name}.hash"));

    // Warm gate checks BOTH candidate locations (destination depends on the
    // output, which only exists after a build — D1-PLAN).
    for c in [&beside, &data_out] {
        if warm(c, &hash) {
            return (c.clone(), Outcome::Warm(c.clone()));
        }
    }

    // Build (CPU — caller has us on spawn_blocking).
    let rss_before = rss_kb();
    let t0 = std::time::Instant::now();
    let code = match bundle(&work) {
        Ok(c) => c,
        Err(e) => {
            return (
                raw.clone(),
                Outcome::Fallback {
                    path: raw,
                    err: format!("bundle: {e:#}"),
                },
            );
        }
    };

    // Destination rule (D1-PLAN conditional emit).
    let needs = needs_ancestry(&code);
    let (dest, other) = if needs {
        (beside.clone(), Some(data_out.clone()))
    } else {
        (data_out.clone(), Some(beside.clone()))
    };

    if let Err(e) = atomic_write(&dest, &code) {
        return (
            raw.clone(),
            Outcome::Fallback {
                path: raw,
                err: format!("write: {e:#}"),
            },
        );
    }
    // Hash AFTER the rename: any crash prefix leaves hash≠content → rebuild.
    if let Err(e) = atomic_write(&hash_path_of(&dest), &hash) {
        tracing::warn!("normalize hash write failed (will rebuild next boot): {e:#}");
    }
    // Stale opposite-location pair (destination flipped / spike leftovers):
    // best-effort remove — these names are ocserve-derived, never user files.
    if let Some(o) = other
        && o != dest
    {
        let _ = std::fs::remove_file(&o);
        let _ = std::fs::remove_file(hash_path_of(&o));
    }
    sweep_stale_tmp(&dest);
    let _ = hash_file; // (path already handled via hash_path_of(dest))

    (
        dest.clone(),
        Outcome::Built {
            path: dest,
            ms: t0.elapsed().as_millis(),
            beside: needs,
            rss_before_kb: rss_before,
            rss_after_kb: rss_kb(),
        },
    )
}

// ---------------------------------------------------------------------------
// Tests — every behavior test holds ENV_LOCK (kill-switch env is
// process-global; guard-rule-7 class of race, same hazard).
// ---------------------------------------------------------------------------
#[cfg(test)]
mod tests {
    use super::*;

    static ENV_LOCK: parking_lot::Mutex<()> = parking_lot::const_mutex(());

    /// M4b idiom: real-plugin tests exist only where the artifact exists.
    fn real_entry(pkg: &str, rel: &str) -> Option<PathBuf> {
        let home = std::env::var("HOME").ok()?;
        let p = PathBuf::from(home).join(pkg).join(rel);
        p.exists().then_some(p)
    }

    fn scratch() -> (PathBuf, PathBuf) {
        let base = std::env::temp_dir().join(format!(
            "ocserve-norm-test-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();
        let data = base.join("data");
        std::fs::create_dir_all(&data).unwrap();
        (base, data)
    }

    fn write_entry(base: &Path, name: &str, body: &str) -> PathBuf {
        let p = base.join(name);
        std::fs::write(&p, body).unwrap();
        p
    }

    // ---- scanner controls (the conditional-emit oracle) ----

    #[test]
    fn scanner_real_magic_context_needs_ancestry() {
        let Some(e) = real_entry(
            ".cache/opencode/packages/@cortexkit/opencode-magic-context@latest/node_modules/@cortexkit/opencode-magic-context",
            "dist/index.js",
        ) else {
            eprintln!("skip: magic-context not in opencode cache");
            return;
        };
        let code = bundle(&e).expect("bundle magic-context");
        assert!(
            needs_ancestry(&code),
            "computed @huggingface/transformers import MUST classify needs-ancestry \
             (this is the ground truth that catches metadata-based scanners)"
        );
    }

    #[test]
    fn scanner_real_reliary8_is_clean() {
        let home = std::env::var("HOME").unwrap();
        let e = PathBuf::from(home).join("src/reliary8/opencode-plugin/dist/index.js");
        if !e.exists() {
            eprintln!("skip: reliary8 plugin absent");
            return;
        }
        let code = bundle(&e).expect("bundle reliary8");
        assert!(
            !needs_ancestry(&code),
            "reliary8 (builtins only) must be clean → data-dir emit, no repo writes"
        );
    }

    #[test]
    fn scanner_real_codex_auth_needs_ancestry() {
        // Ground truth FLIPPED 2026-10-06 by evidence: codex-auth's
        // getLockFunction() does `await import(specifier)` with specifier =
        // "proper-lockfile" (its package.json dep) — a runtime-resolved
        // BARE package → beside-entry emit is REQUIRED (cache ancestry).
        // The first "clean" assumption was wrong; the scanner was right.
        let Some(e) = real_entry(
            ".cache/opencode/packages/@iam-brain/opencode-codex-auth@latest/node_modules/@iam-brain/opencode-codex-auth",
            "dist/index.js",
        ) else {
            eprintln!("skip: codex-auth not in opencode cache");
            return;
        };
        let code = bundle(&e).expect("bundle codex-auth");
        assert!(
            needs_ancestry(&code),
            "codex-auth dynamically imports proper-lockfile at runtime"
        );
    }

    #[test]
    fn scanner_shim_identifier_stays_clean() {
        // The K5b shim (inlined into every bun:sqlite bundle) does
        // `await import(BUN_SPEC)` where `const BUN_SPEC = "bun:sqlite"`
        // — virtual-module code is NOT folded at the call site (production
        // evidence: magic-context output keeps the identifier). Staying
        // clean REQUIRES const_map: gut it → None → false-dirty → any
        // future path-plugin using bun:sqlite would pollute its repo.
        let (base, _d) = scratch();
        let e = write_entry(
            &base,
            "shimuser.js",
            r#"import { Database } from "bun:sqlite";
export default { Database };"#,
        );
        let code = bundle(&e).expect("bundle shim user");
        assert!(
            !needs_ancestry(&code),
            "shim import(BUN_SPEC) must resolve via const_map to bun:sqlite (scheme-clean)"
        );
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn scanner_synthetic_computed_import_needs_ancestry() {
        let (base, _d) = scratch();
        let e = write_entry(
            &base,
            "dyn.js",
            r#"const pkg = "some-dep" + "-x";
export default await import(pkg);"#,
        );
        let code = bundle(&e).expect("bundle synthetic dyn");
        assert!(needs_ancestry(&code));
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn scanner_node_relative_const_are_clean() {
        let (base, _d) = scratch();
        std::fs::write(base.join("sib.js"), "export const x = 1;\n").unwrap();
        // const via concat (real-bundle shape: rolldown keeps `import(P)`,
        // folding the concat into `const P = "node:crypto"` — pure literals
        // get inlined at the call and would bypass const_map entirely)
        let e = write_entry(
            &base,
            "ok.js",
            r#"import fs from "node:fs";
import path2 from "path";
import { x } from "./sib.js";
const P = "node:" + "crypto";
const dyn = await import(P);
export default { fs, path2, x, dyn };"#,
        );
        let code = bundle(&e).expect("bundle clean fixture");
        assert!(
            !needs_ancestry(&code),
            "node:/relative/builtin + const-resolved node: spec are clean"
        );
        let _ = std::fs::remove_dir_all(&base);
    }

    // ---- behavior (all under ENV_LOCK — env is process-global) ----

    #[test]
    fn warm_hash_skips_rebuild() {
        let _g = ENV_LOCK.lock();
        // SAFETY: process-global env mutation, serialized under ENV_LOCK, restored here.
        unsafe {
            std::env::remove_var("OCSERVE_PLUGIN_NORMALIZE");
        };
        let (base, data) = scratch();
        let e = write_entry(&base, "p.js", "export default 1;\n");
        let (p1, o1) = normalize_or_raw_sync(Some(&data), &e);
        assert!(matches!(o1, Outcome::Built { .. }), "first: {o1:?}");
        let (p2, o2) = normalize_or_raw_sync(Some(&data), &e);
        assert!(matches!(o2, Outcome::Warm(_)), "second: {o2:?}");
        assert_eq!(p1, p2);
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn stale_entry_rebuilds() {
        let _g = ENV_LOCK.lock();
        // SAFETY: process-global env mutation, serialized under ENV_LOCK, restored here.
        unsafe {
            std::env::remove_var("OCSERVE_PLUGIN_NORMALIZE");
        };
        let (base, data) = scratch();
        let e = write_entry(&base, "p.js", "export default 1;\n");
        let (_, o1) = normalize_or_raw_sync(Some(&data), &e);
        assert!(matches!(o1, Outcome::Built { .. }));
        std::fs::write(&e, "export default 2; // changed\n").unwrap();
        let (_, o2) = normalize_or_raw_sync(Some(&data), &e);
        assert!(matches!(o2, Outcome::Built { .. }), "changed: {o2:?}");
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn normalize_error_falls_back_to_raw() {
        let _g = ENV_LOCK.lock();
        // SAFETY: process-global env mutation, serialized under ENV_LOCK, restored here.
        unsafe {
            std::env::remove_var("OCSERVE_PLUGIN_NORMALIZE");
        };
        let (base, data) = scratch();
        let e = write_entry(&base, "bad.js", "import { from 'broken");
        let (p, o) = normalize_or_raw_sync(Some(&data), &e);
        assert_eq!(p, e, "fallback path MUST be the raw entry");
        assert!(matches!(o, Outcome::Fallback { .. }), "{o:?}");
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn kill_switch_disables_building() {
        let _g = ENV_LOCK.lock();
        let (base, data) = scratch();
        let e = write_entry(&base, "p.js", "export default 1;\n");
        // SAFETY: single env mutation under the shared lock, restored below.
        unsafe { std::env::set_var("OCSERVE_PLUGIN_NORMALIZE", "0") };
        let (p, o) = normalize_or_raw_sync(Some(&data), &e);
        // SAFETY: process-global env mutation, serialized under ENV_LOCK, restored here.
        unsafe {
            std::env::remove_var("OCSERVE_PLUGIN_NORMALIZE");
        };
        assert_eq!(p, e);
        assert!(matches!(o, Outcome::Skipped(_)), "{o:?}");
        // proof of no-build: normalized dir stays empty
        let built = std::fs::read_dir(&data).map(|d| d.count()).unwrap_or(0);
        assert_eq!(built, 0, "kill switch must not construct outputs");
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn wipe_then_normalize_rebuilds() {
        let _g = ENV_LOCK.lock();
        // SAFETY: process-global env mutation, serialized under ENV_LOCK, restored here.
        unsafe {
            std::env::remove_var("OCSERVE_PLUGIN_NORMALIZE");
        };
        let (base, data) = scratch();
        let e = write_entry(&base, "p.js", "export default 1;\n");
        let (p1, o1) = normalize_or_raw_sync(Some(&data), &e);
        assert!(matches!(o1, Outcome::Built { .. }));
        std::fs::remove_file(&p1).unwrap();
        std::fs::remove_file(hash_path_of(&p1)).unwrap();
        let (p2, o2) = normalize_or_raw_sync(Some(&data), &e);
        assert!(matches!(o2, Outcome::Built { .. }), "after wipe: {o2:?}");
        assert!(p2.exists());
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn suffix_guard_passthrough() {
        let _g = ENV_LOCK.lock();
        // SAFETY: process-global env mutation, serialized under ENV_LOCK, restored here.
        unsafe {
            std::env::remove_var("OCSERVE_PLUGIN_NORMALIZE");
        };
        let (base, data) = scratch();
        let already = base.join("thing.normalized.mjs");
        std::fs::write(&already, "// sentinel, must not be re-bundled\n").unwrap();
        let (p, o) = normalize_or_raw_sync(Some(&data), &already);
        assert_eq!(p, already);
        assert!(matches!(o, Outcome::Passthrough(_)), "{o:?}");
        let after = std::fs::read_to_string(&already).unwrap();
        assert!(after.contains("sentinel"), "output must be untouched");
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn dest_routing_clean_goes_to_data_dir() {
        let _g = ENV_LOCK.lock();
        // SAFETY: process-global env mutation, serialized under ENV_LOCK, restored here.
        unsafe {
            std::env::remove_var("OCSERVE_PLUGIN_NORMALIZE");
        };
        let (base, data) = scratch();
        let e = write_entry(&base, "plain.js", "export default 42;\n");
        let (p, o) = normalize_or_raw_sync(Some(&data), &e);
        assert!(matches!(o, Outcome::Built { beside: false, .. }), "{o:?}");
        assert!(
            p.starts_with(&data),
            "self-contained bundle must emit into ocserve's data dir, not beside entry: {}",
            p.display()
        );
        // and the beside-entry candidate was never created
        assert!(!base.join("plain.normalized.mjs").exists());
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn dest_routing_needs_ancestry_goes_beside_entry() {
        let _g = ENV_LOCK.lock();
        // SAFETY: process-global env mutation, serialized under ENV_LOCK, restored here.
        unsafe {
            std::env::remove_var("OCSERVE_PLUGIN_NORMALIZE");
        };
        let (base, data) = scratch();
        let e = write_entry(
            &base,
            "dyn.js",
            r#"const pkg = "some-dep" + "-x";
export default await import(pkg);"#,
        );
        let (p, o) = normalize_or_raw_sync(Some(&data), &e);
        assert!(matches!(o, Outcome::Built { beside: true, .. }), "{o:?}");
        assert_eq!(p, base.join("dyn.normalized.mjs"), "K4: beside entry");
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn flip_destination_removes_stale_pair() {
        let _g = ENV_LOCK.lock();
        // SAFETY: process-global env mutation, serialized under ENV_LOCK, restored here.
        unsafe {
            std::env::remove_var("OCSERVE_PLUGIN_NORMALIZE");
        };
        let (base, data) = scratch();
        let e = write_entry(&base, "flip.js", "export default 7;\n");
        let (p, _) = normalize_or_raw_sync(Some(&data), &e);
        assert!(p.starts_with(&data), "clean → data dir first");
        // Plant a stale beside-entry pair (as if a previous rule emitted there)
        let stale = base.join("flip.normalized.mjs");
        std::fs::write(&stale, "// stale\n").unwrap();
        // Content change → rebuild → dest still data-dir → stale pair removed
        std::fs::write(&e, "export default 8;\n").unwrap();
        let (p2, o2) = normalize_or_raw_sync(Some(&data), &e);
        assert!(matches!(o2, Outcome::Built { beside: false, .. }), "{o2:?}");
        assert!(p2.starts_with(&data));
        assert!(
            !stale.exists(),
            "stale beside-entry pair must be cleaned (spike-leftover class)"
        );
        let _ = std::fs::remove_dir_all(&base);
    }
}
