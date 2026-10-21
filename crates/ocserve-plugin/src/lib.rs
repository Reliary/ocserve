//! ocserve-plugin: opencode plugin compatibility via a Node sidecar (M4b).
//!
//! Gate rationale (PLAN §10, evidence not assumption): the frozen plugins
//! import `bun:sqlite`, `child_process`, `node:fs/crypto/url` and TTY UI
//! modules (import audit of the actual artifacts). A quickjs embed would need
//! a dozen shims with a *synchronous* SQLite bridge — rejected as high-risk;
//! one Node sidecar (v25 present, `node:sqlite` built in) is the low-
//! maintenance host. Isolation: sidecar crash/restart cannot take down the
//! server; protocol = NDJSON on stdio; plugin chatter is forced to stderr by
//! the host so framing cannot corrupt.
#![cfg_attr(not(test), forbid(unsafe_code))]

pub mod normalize;
pub use normalize::{Outcome as NormalizeOutcome, normalize_or_raw_sync};

use anyhow::{Context, Result, anyhow};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::io::{AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, Command};
use tokio::sync::oneshot;

/// Host files (written next to the data dir once; content-addressed by embed).
pub const HOST_FILES: &[(&str, &str)] = &[
    ("host.mjs", include_str!("host/host.mjs")),
    ("v2.mjs", include_str!("host/v2.mjs")),
    (
        "bun-sqlite-loader.mjs",
        include_str!("host/bun-sqlite-loader.mjs"),
    ),
    (
        "shim-bun-sqlite.mjs",
        include_str!("host/shim-bun-sqlite.mjs"),
    ),
    (
        "shim-bun-sqlite.cjs",
        include_str!("host/shim-bun-sqlite.cjs"),
    ),
];

/// Per-RPC timeout (hooks are local JS; anything longer is a runaway plugin).
pub const RPC_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

/// Write host files into `dir` (idempotent overwrite — embed is authoritative).
pub fn materialize_host(dir: &Path) -> Result<PathBuf> {
    std::fs::create_dir_all(dir).with_context(|| format!("create host dir {}", dir.display()))?;
    for (name, body) in HOST_FILES {
        let path = dir.join(name);
        std::fs::write(&path, body).with_context(|| format!("write {}", path.display()))?;
    }
    Ok(dir.join("host.mjs"))
}

/// Resolve a plugin spec to an entry file (loader.ts semantics, npm-install
/// stage assumed already done by opencode's cache — we re-use it read-only):
/// - absolute/relative path → direct entry
/// - npm spec (`name`, `@scope/name`, `name@version`) →
///   `~/.cache/opencode/packages/{spec}[@latest]/node_modules/{pkg}/…` entry
///   via package.json exports["."] | main | module
pub fn resolve_entry(spec: &str, home: &Path) -> Result<PathBuf> {
    if spec.starts_with('/') || spec.starts_with("./") || spec.starts_with("../") {
        let p = PathBuf::from(spec);
        if p.exists() {
            return Ok(p);
        }
        anyhow::bail!("plugin file not found: {spec}");
    }
    let cache = home.join(".cache/opencode/packages");
    let pkg_name = strip_version(spec);
    let candidates = [
        cache.join(format!("{spec}@latest")),
        cache.join(spec),
        cache.join(format!("{pkg_name}@latest")),
    ];
    for dir in &candidates {
        if !dir.exists() {
            continue;
        }
        // wrapper package.json → node_modules/<pkg>/package.json
        let direct = dir.join("package.json");
        if let Some(entry) = entry_from_package(&direct, dir, &pkg_name) {
            return Ok(entry);
        }
        let nested = dir
            .join("node_modules")
            .join(&pkg_name)
            .join("package.json");
        if let Some(entry) = entry_from_package(&nested, nested.parent().unwrap_or(dir), &pkg_name)
        {
            return Ok(entry);
        }
    }
    anyhow::bail!("plugin target not found in opencode cache: {spec}")
}

fn strip_version(spec: &str) -> String {
    // "@scope/name@latest" → "@scope/name"; "name@1.2.3" → "name"
    let bytes = spec.as_bytes();
    // find the LAST '@' that is NOT at index 0 (scope marker)
    let mut cut = None;
    for (i, b) in bytes.iter().enumerate() {
        if *b == b'@' && i > 0 {
            cut = Some(i);
        }
    }
    match cut {
        Some(i) => spec[..i].to_string(),
        None => spec.to_string(),
    }
}

fn entry_from_package(pkg_json: &Path, base: &Path, _pkg_name: &str) -> Option<PathBuf> {
    let text = std::fs::read_to_string(pkg_json).ok()?;
    let v: Value = serde_json::from_str(&text).ok()?;
    let mut rel: Option<&str> = None;
    if let Some(exp) = v.get("exports")
        && let Some(dot) = exp.get(".")
    {
        rel = match dot {
            Value::String(s) => Some(s.as_str()),
            Value::Object(_) => dot
                .get("import")
                .and_then(|x| x.as_str())
                .or_else(|| dot.get("default").and_then(|x| x.as_str())),
            _ => None,
        };
    }
    rel = rel.or_else(|| v.get("main").and_then(|x| x.as_str()));
    rel = rel.or_else(|| v.get("module").and_then(|x| x.as_str()));
    let rel = rel?;
    let entry = base.join(rel);
    // G6: the package's own `exports`/`main`/`module` may point outside the
    // package (e.g. `../../other`). Lexically normalize and require the entry
    // to stay inside `base`; a plugin entry is loaded only from its package.
    let lex = lexically_normalize(&entry);
    if !lex.starts_with(base)
        && !std::fs::canonicalize(base)
            .ok()
            .is_some_and(|cb| lex.starts_with(&cb))
    {
        tracing::warn!("plugin entry {rel} escapes its package dir — refused");
        return None;
    }
    lex.exists().then_some(lex)
}

fn lexically_normalize(p: &Path) -> PathBuf {
    use std::path::Component;
    let mut out = PathBuf::new();
    for c in p.components() {
        match c {
            Component::ParentDir => {
                out.pop();
            }
            Component::CurDir => {}
            other => out.push(other.as_os_str()),
        }
    }
    out
}

struct Pending {
    tx: oneshot::Sender<Result<Value, String>>,
}

/// Sidecar handle: one Node process, NDJSON request/response, reader task.
pub struct Sidecar {
    stdin: ChildStdin,
    pending: Arc<parking_lot::Mutex<HashMap<i64, Pending>>>,
    next_id: std::sync::atomic::AtomicI64,
    child: Child,
    /// load results (spec → hooks) for status introspection
    statuses: parking_lot::Mutex<HashMap<String, Value>>,
    /// server/directory context for loads (kept for future reloads)
    server: (String, String),
    /// spawn identity for respawn-after-death
    args: SpawnArgs,
    /// successful loads (spec, entry, input, options) — replayed on respawn
    /// (entries stored RAW; load_raw re-normalizes on replay — D1-PLAN).
    /// `options` is the v1 `[spec, options]` tuple's second element (Null when
    /// the plain-string form was used) — passed to the server factory.
    loads: Vec<(String, PathBuf, Value, Value)>,
    /// normalization output root (`data/normalized`); None = skip (tests,
    /// any sidecar without it — existing behavior untouched, D1-PLAN)
    normalize_root: Option<PathBuf>,
    /// spawn time for the L1 recycle min-uptime guard (SRE §5) — fresh on
    /// every (re)spawn, so a recycled sidecar can't be recycled again until
    /// the guard window passes.
    started_at: std::time::Instant,
    /// L1 recycle marker: set when WE killed the child on purpose; the next
    /// ensure_alive must log that as INFO (planned), not ERROR (crash) —
    /// an intentional action must never read as a failure in the logs.
    recycled: bool,
}

/// Plugin-host runtime: Bun preferred (upstream runs plugins under Bun; real
/// `bun:sqlite`, TS plugins load, lower RSS floor), Node fallback.
/// `OCSERVE_PLUGIN_RUNTIME=node|bun` forces one (lab A/B control).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PluginRuntime {
    Bun,
    Node,
}

impl PluginRuntime {
    pub fn program(self) -> &'static str {
        match self {
            PluginRuntime::Bun => "bun",
            PluginRuntime::Node => "node",
        }
    }
}

/// Resolve an executable on PATH plus `~/.bun/bin` (the common user-local
/// bun install location — systemd units do not see shell PATH extras).
pub(crate) fn which(prog: &str) -> Option<PathBuf> {
    // Windows resolves executables by suffix; `Path::join("bun")` never finds
    // `bun.exe` (PLATFORM.md §1.2). The suffix lives in one place — ocserve-core
    // — so the plugin host, the MCP client and the tools cannot drift.
    let name = ocserve_store::platform::exe(prog);
    if let Ok(path) = std::env::var("PATH") {
        for dir in std::env::split_paths(&path) {
            let cand = dir.join(&name);
            if ocserve_store::platform::is_executable_file(&cand) {
                return Some(cand);
            }
        }
    }
    for key in ["OCSERVE_HOME", "HOME", "USERPROFILE"] {
        let Some(home) = std::env::var_os(key).filter(|v| !v.is_empty()) else {
            continue;
        };
        let cand = Path::new(&home).join(".bun/bin").join(&name);
        if ocserve_store::platform::is_executable_file(&cand) {
            return Some(cand);
        }
    }
    None
}

pub fn plugin_runtime() -> PluginRuntime {
    match std::env::var("OCSERVE_PLUGIN_RUNTIME").as_deref() {
        Ok("node") => return PluginRuntime::Node,
        Ok("bun") => return PluginRuntime::Bun,
        _ => {}
    }
    if which("bun").is_some() {
        PluginRuntime::Bun
    } else {
        PluginRuntime::Node
    }
}

#[derive(Clone)]
struct SpawnArgs {
    host_path: PathBuf,
    server_url: String,
    directory: String,
}

impl Sidecar {
    pub async fn spawn(host_path: &Path, server_url: &str, directory: &str) -> Result<Self> {
        // A3 (OOM-resilience): same wrapper as ocserve-mcp — the plugin host
        // dies first on memcg pressure (ensure_alive respawns it) instead
        // of the main service.
        let runtime = crate::plugin_runtime();
        // absolute path (systemd PATH may lack ~/.bun/bin); fall back to the
        // bare name so a PATH-only environment still resolves
        let program = which(runtime.program())
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_else(|| runtime.program().to_string());
        // Containment (SRE §5.2): the child moves itself into kids/plugin and
        // raises its oom_score_adj before exec, so a memcg kill takes the
        // sidecar (ensure_alive respawns it) instead of the server.
        let shape = ocserve_store::cgroup::contained(&program, "plugin");
        let mut cmd = Command::new(&shape.program);
        cmd.args(&shape.prefix);
        if let Some(p) = &shape.cgroup {
            cmd.env(ocserve_store::cgroup::ENV, p);
        }
        match runtime {
            // Bun (upstream's own plugin runtime): real bun:sqlite + TS
            // plugins load; `--smol` trades GC frequency for a lower RSS
            // floor. No V8 --max-old-space knobs exist here.
            PluginRuntime::Bun => {
                cmd.arg("--smol");
            }
            // Node (fallback when bun is absent): V8 heap cap (env-tunable).
            // Default raised64→128 after the LIVE battery (2026-10-03): with
            // the real plugin set the sidecar OOM-killed at64 (observed boot
            // death → hooks broken-piped until restart). Measured
            // 64/128/192/256: RSS flat ~145-148MB either way (native
            // dominates; heap cap only binds when heap actually grows).
            PluginRuntime::Node => {
                cmd.arg(format!(
                    "--max-old-space-size={}",
                    std::env::var("OCSERVE_PLUGIN_HEAP_MB")
                        .ok()
                        .and_then(|v| v.parse().ok())
                        .unwrap_or(128)
                ));
                cmd.arg("--max-semi-space-size=2");
            }
        }
        cmd.arg(host_path)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::inherit())
            .kill_on_drop(true);
        let mut child = cmd
            .spawn()
            .with_context(|| format!("spawn {} plugin host", runtime.program()))?;
        let stdin = child.stdin.take().context("host stdin")?;
        let stdout = child.stdout.take().context("host stdout")?;
        let pending = Arc::new(parking_lot::Mutex::new(HashMap::<i64, Pending>::new()));
        let pending_task = Arc::clone(&pending);
        tokio::spawn(async move {
            // Bounded line reads: a plugin that prints a giant line (no newline)
            // would otherwise grow `line` unbounded (host.mjs stdout is reserved
            // for RPC, but a runaway plugin can still emit garbage). Cap at the
            // RPC line budget; a longer line is discarded.
            const MAX_HOST_LINE: usize = 8 * 1024 * 1024;
            let mut reader = BufReader::new(stdout);
            let mut line = String::new();
            loop {
                line.clear();
                use tokio::io::AsyncBufReadExt as _;
                let n = {
                    let mut limited =
                        tokio::io::AsyncReadExt::take(&mut reader, MAX_HOST_LINE as u64 + 1);
                    limited.read_line(&mut line).await
                };
                match n {
                    Ok(0) | Err(_) => break,
                    // oversized: the rest of the line is unreadable framing —
                    // stop; in-flight RPCs fail on timeout (never hang the pump).
                    Ok(_) if line.len() > MAX_HOST_LINE => {
                        tracing::warn!("plugin host: oversized line — reader stopped");
                        break;
                    }
                    Ok(_) => {}
                }
                let trimmed = line.trim();
                if trimmed.is_empty() {
                    continue;
                }
                match serde_json::from_str::<Value>(trimmed) {
                    Ok(v) => {
                        if let Some(id) = v.get("id").and_then(|i| i.as_i64()) {
                            let result = match v.get("error") {
                                Some(e) if !e.is_null() => {
                                    Err(e["message"].as_str().unwrap_or("host error").to_string())
                                }
                                _ => Ok(v.get("result").cloned().unwrap_or(Value::Null)),
                            };
                            if let Some(p) = pending_task.lock().remove(&id) {
                                let _ = p.tx.send(result);
                            }
                        }
                        // no id → informational; host routes plugin chatter to stderr
                    }
                    Err(_) => {
                        let head: String = trimmed.chars().take(120).collect();
                        tracing::warn!("plugin host: unparsable line: {head}");
                    }
                }
            }
            // host death: fail everything in flight (fail fast, AGENTS §2.4)
            for (_, p) in pending_task.lock().drain() {
                let _ = p.tx.send(Err("plugin host exited".into()));
            }
        });
        Ok(Self {
            stdin,
            pending,
            next_id: std::sync::atomic::AtomicI64::new(1),
            child,
            statuses: parking_lot::Mutex::new(HashMap::new()),
            server: (server_url.to_string(), directory.to_string()),
            args: SpawnArgs {
                host_path: host_path.to_path_buf(),
                server_url: server_url.to_string(),
                directory: directory.to_string(),
            },
            loads: Vec::new(),
            normalize_root: None,
            started_at: std::time::Instant::now(),
            recycled: false,
        })
    }

    /// RPCs currently in flight (the L1 recycle idle gate — a hook must
    /// never be interrupted mid-call).
    pub fn in_flight(&self) -> usize {
        self.pending.lock().len()
    }

    /// Seconds since this sidecar instance was spawned (respawns reset it).
    pub fn uptime(&self) -> std::time::Duration {
        self.started_at.elapsed()
    }

    /// L1 graceful recycle (SRE §5): kill the sidecar child while idle.
    /// State is disk-backed (context.db / pins) and `loads` is kept, so the
    /// next `ensure_alive` respawns + replays hooks (warm-hash normalize is
    /// a no-op). SIGKILL is deliberate: the host has no shutdown protocol
    /// and nothing in it needs flushing. Returns false if already dead.
    pub fn recycle_child(&mut self) -> bool {
        if self.child.try_wait().ok().flatten().is_some() {
            return false;
        }
        let _ = self.child.start_kill();
        self.statuses.lock().clear();
        self.recycled = true;
        true
    }

    /// Enable normalization for this sidecar (set once at boot; carried to
    /// respawns by `ensure_alive`). None/never-set = raw loads (today's
    /// behavior — all existing tests stay on this path).
    pub fn set_normalize_root(&mut self, root: PathBuf) {
        self.normalize_root = Some(root);
    }

    /// Respawn after unexpected death (V8 OOM / crash) and replay every
    /// previously loaded plugin. Without this a dead sidecar leaves ALL
    /// hooks broken-piped until service restart (live battery finding
    /// 2026-10-03: boot-time heap OOM, fail-open hid it in the prompt path).
    pub async fn ensure_alive(&mut self) -> Result<()> {
        match self.child.try_wait() {
            Ok(None) => return Ok(()),
            Ok(Some(status)) => {
                if std::mem::take(&mut self.recycled) {
                    tracing::info!(
                        "plugin sidecar recycled (L1, was {status}) — respawning + replaying hooks"
                    );
                } else {
                    tracing::error!("plugin sidecar exited ({status}) — respawning");
                }
            }
            Err(e) => {
                tracing::error!("plugin sidecar try_wait failed: {e} — respawning");
            }
        }
        let args = self.args.clone();
        let loads = std::mem::take(&mut self.loads);
        // Spawn failure must NOT lose `loads`: `self` is only replaced on
        // success, so restore the taken entries and surface the error. Without
        // this the plugin set is silently empty after a failed respawn — hooks
        // never replay and `in_flight()` stays zero (statuses stale).
        let mut fresh =
            match Sidecar::spawn(&args.host_path, &args.server_url, &args.directory).await {
                Ok(f) => f,
                Err(e) => {
                    self.loads = loads;
                    tracing::error!("plugin sidecar respawn failed: {e:#} — plugin set preserved");
                    return Err(e);
                }
            };
        // D1-PLAN: replay must normalize too (warm no-op; rebuilds if outputs
        // were wiped mid-run — A1 stores raw, rebuilds here AND at next boot).
        fresh.normalize_root = self.normalize_root.clone();
        for (spec, entry, input, options) in &loads {
            if let Err(e) = fresh.load_raw(spec, entry, input, options).await {
                tracing::warn!("plugin {spec} reload after respawn failed: {e:#}");
            }
        }
        *self = fresh;
        Ok(())
    }

    /// Child pid (test/diag: force-kill to prove respawn).
    pub fn child_pid(&self) -> u32 {
        self.child.id().unwrap_or(0)
    }

    pub async fn request(&mut self, method: &str, params: Value) -> Result<Value> {
        self.ensure_alive().await?;
        self.request_raw(method, params).await
    }

    /// Unguarded RPC (respawn replay only — the fresh child is alive).
    async fn request_raw(&mut self, method: &str, params: Value) -> Result<Value> {
        let id = self
            .next_id
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        self.pending.lock().insert(id, Pending { tx });
        let msg = json!({"id": id, "method": method, "params": params});
        let mut line = msg.to_string();
        line.push('\n');
        // A write failure must remove the pending entry or `in_flight()` (the L1
        // recycle idle gate) never returns to zero and the sidecar can never be
        // recycled. Clean up before propagating.
        if let Err(e) = self.stdin.write_all(line.as_bytes()).await {
            self.pending.lock().remove(&id);
            return Err(e).context("plugin host stdin write");
        }
        if let Err(e) = self.stdin.flush().await {
            self.pending.lock().remove(&id);
            return Err(e).context("plugin host stdin flush");
        }
        match tokio::time::timeout(RPC_TIMEOUT, rx).await {
            Ok(Ok(res)) => res.map_err(|e| anyhow!("plugin host {method}: {e}")),
            Ok(Err(_)) => Err(anyhow!("plugin host dropped response for {method}")),
            Err(_) => {
                self.pending.lock().remove(&id);
                Err(anyhow!("plugin host {method} timed out (runaway hook?)"))
            }
        }
    }

    /// Load one plugin (v1 server-factory extraction happens host-side).
    /// `options` is the v1 `[spec, options]` tuple's second element (pass
    /// `Value::Null` for the plain-string form) — delivered to the factory.
    pub async fn load(
        &mut self,
        spec: &str,
        entry: &Path,
        input: &Value,
        options: &Value,
    ) -> Result<Vec<String>> {
        self.ensure_alive().await?;
        self.load_raw(spec, entry, input, options).await
    }

    /// Unguarded load (respawn replay only — fresh child is alive).
    async fn load_raw(
        &mut self,
        spec: &str,
        entry: &Path,
        input: &Value,
        options: &Value,
    ) -> Result<Vec<String>> {
        // D1: normalize for the RPC (CPU on spawn_blocking — A2 class); the
        // RAW entry is what gets recorded for respawn replay (A1).
        let root = self.normalize_root.clone();
        let raw = entry.to_path_buf();
        let (load_path, outcome) = tokio::task::spawn_blocking(move || {
            crate::normalize::normalize_or_raw_sync(root.as_deref(), &raw)
        })
        .await
        .map_err(|e| anyhow!("normalize task: {e}"))?;
        ocserve_metrics::labeled_counter(
            "ocserve_plugin_normalize_total",
            &format!("result=\"{}\"", outcome.label()),
            1,
        );
        match &outcome {
            crate::normalize::Outcome::Built {
                path,
                ms,
                beside,
                rss_before_kb,
                rss_after_kb,
            } => tracing::info!(
                "plugin normalize {spec}: built {} ({}) in {ms}ms rss {rss_before_kb}→{rss_after_kb}kB",
                path.display(),
                if *beside { "beside-entry" } else { "data-dir" }
            ),
            crate::normalize::Outcome::Fallback { err, .. } => {
                tracing::warn!("plugin normalize {spec} failed, loading raw entry: {err}")
            }
            _ => {}
        }
        // server/directory context defaults from spawn (input may omit them)
        let mut input = input.clone();
        if input
            .get("serverUrl")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .is_empty()
        {
            input["serverUrl"] = json!(self.server.0);
        }
        if input
            .get("directory")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .is_empty()
        {
            input["directory"] = json!(self.server.1);
        }
        let params = json!({
            "spec": spec,
            "entry": load_path.to_string_lossy(),
            "input": input,
            "options": options,
        });
        let res = self.request_raw("load", params).await?;
        let hooks: Vec<String> = res["hooks"]
            .as_array()
            .map(|a| {
                a.iter()
                    .filter_map(|h| h.as_str().map(String::from))
                    .collect()
            })
            .unwrap_or_default();
        self.statuses.lock().insert(
            spec.to_string(),
            json!({"ok": true, "hooks": hooks, "id": res["id"]}),
        );
        self.loads.push((
            spec.to_string(),
            entry.to_path_buf(),
            input,
            options.clone(),
        ));
        Ok(hooks)
    }

    /// v1 trigger semantics: sequential hooks mutate `output` in place.
    pub async fn trigger(&mut self, name: &str, input: Value, mut output: Value) -> Result<Value> {
        let params = json!({"name": name, "input": input, "output": output});
        let res = self.request("trigger", params).await?;
        if res.is_object() {
            output = res;
        }
        Ok(output)
    }

    /// v1 event delivery (plugin/index.ts:255-259): per-plugin
    /// `hooks["event"]({event})`. Returns host delivery count.
    pub async fn emit_event(&mut self, event: Value) -> Result<Value> {
        self.request("event", json!({"event": event})).await
    }

    /// v2 domain-hook dispatch (Phase 3): v2 hooks mutate the event object in
    /// place (like v1's output), so this returns `{output, matched}`. `matched`
    /// = 0 means no v2 plugin installed this hook (caller skips mutation).
    pub async fn trigger_v2(&mut self, domain: &str, name: &str, input: Value) -> Result<Value> {
        self.request(
            "trigger_v2",
            json!({"domain": domain, "name": name, "input": input}),
        )
        .await
    }

    /// v2 RPC dispatch (Phase 3): `{rpcID, method, input}` →
    /// `{output}` (success) or `{error:{type,message}}` (unavailable /
    /// method_not_found / handler_error).
    pub async fn rpc_call(&mut self, rpc_id: &str, method: &str, input: Value) -> Result<Value> {
        self.request(
            "rpc_call",
            json!({"rpcID": rpc_id, "method": method, "input": input}),
        )
        .await
    }

    /// v2 plugin inventory (real — id/dialect per loaded v2 plugin + the
    /// registered RPC ids). Powers `/api/plugin` without fabricating a list.
    pub async fn v2_inventory(&mut self) -> Result<Value> {
        self.request("v2_inventory", json!({})).await
    }

    /// Run every registered v2 `command.transform` / `skill.transform` over
    /// `commands` and return the edited list.
    ///
    /// PULLED by the caller on config reload (never pushed by the plugin), so
    /// a payload can only ever serve a list this call actually returned — the
    /// property that kills the push-race where a stale list is served after a
    /// reconcile that never asked. Upstream semantics mirrored from
    /// `State.create` (core/state.ts:172-194): transforms start from the
    /// pristine list each call and a throwing transform is reported, not fatal.
    ///
    /// Returns `(commands, failures)`; a sidecar error propagates so the
    /// caller can fall back to the untransformed list rather than serve an
    /// empty command surface.
    pub async fn transform_commands(
        &mut self,
        commands: Vec<Value>,
    ) -> Result<(Vec<Value>, Vec<Value>)> {
        let r = self
            .request("v2_transform_commands", json!({ "commands": commands }))
            .await?;
        let out = r["commands"]
            .as_array()
            .cloned()
            .unwrap_or_default()
            .into_iter()
            .collect();
        let failures = r["failures"].as_array().cloned().unwrap_or_default();
        Ok((out, failures))
    }

    pub async fn config(&mut self, cfg: Value) -> Result<Value> {
        self.request("config", json!({"config": cfg})).await
    }

    pub fn statuses(&self) -> Value {
        let mut out = serde_json::Map::new();
        for (spec, v) in self.statuses.lock().iter() {
            out.insert(spec.clone(), v.clone());
        }
        Value::Object(out)
    }

    pub async fn shutdown(&mut self) {
        let _ = self.request("dispose", json!({})).await;
        let _ = self.child.start_kill();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strip_version_cases() {
        assert_eq!(strip_version("context-mode"), "context-mode");
        assert_eq!(strip_version("context-mode@latest"), "context-mode");
        assert_eq!(
            strip_version("@cortexkit/opencode-magic-context@latest"),
            "@cortexkit/opencode-magic-context"
        );
        assert_eq!(
            strip_version("@iam-brain/opencode-codex-auth@latest"),
            "@iam-brain/opencode-codex-auth"
        );
    }

    #[test]
    fn entry_from_package_shapes() {
        let dir = tempfile::tempdir().unwrap();
        // exports["."] string
        std::fs::write(
            dir.path().join("package.json"),
            r#"{"exports": {".": "./dist/index.js"}}"#,
        )
        .unwrap();
        std::fs::create_dir(dir.path().join("dist")).unwrap();
        std::fs::write(dir.path().join("dist/index.js"), "//").unwrap();
        let e = entry_from_package(&dir.path().join("package.json"), dir.path(), "x")
            .expect("exports dot");
        assert!(e.ends_with("dist/index.js"));
        // main fallback
        std::fs::write(
            dir.path().join("package.json"),
            r#"{"main": "./lib/main.js"}"#,
        )
        .unwrap();
        std::fs::create_dir(dir.path().join("lib")).unwrap();
        std::fs::write(dir.path().join("lib/main.js"), "//").unwrap();
        let e =
            entry_from_package(&dir.path().join("package.json"), dir.path(), "x").expect("main");
        assert!(e.ends_with("lib/main.js"));
    }

    #[tokio::test]
    async fn sidecar_load_trigger_roundtrip() -> anyhow::Result<()> {
        if !runtime_available() {
            panic!(
                "plugin-runtime binary ({}) required for plugin-host tests (M4b environment gate)",
                crate::plugin_runtime().program()
            );
        }
        let dir = tempfile::tempdir().unwrap();
        // synthetic v1 plugin: PluginModule.server factory + mutating hook
        std::fs::write(
            dir.path().join("plug.mjs"),
            r#"
export const PluginModule = {
  id: "synthetic",
  server: async () => ({
    "tool.execute.after": async (input, output) => { output.seen = input.tool; },
    "chat.message": async (input, output) => { output.touched = true; },
    "event": async (payload) => {
      if (!payload.event || !payload.event.type) throw new Error("bad event shape");
      if (payload.event.type === "boom.type") throw new Error("event boom");
    },
  }),
};
export default PluginModule.server;
"#,
        )
        .unwrap();
        let host = materialize_host(&dir.path().join("host"))?;
        let mut sc = Sidecar::spawn(&host, "http://127.0.0.1:9", "/work")
            .await
            .expect("spawn node");
        let ping = sc.request("ping", json!({})).await.expect("ping");
        assert_eq!(ping, "pong");

        let input =
            json!({"directory": "/work", "projectID": "global", "serverUrl": "http://127.0.0.1:9"});
        let hooks = sc
            .load(
                "synthetic",
                &dir.path().join("plug.mjs"),
                &input,
                &Value::Null,
            )
            .await
            .expect("load");
        assert_eq!(
            hooks,
            vec![
                "tool.execute.after".to_string(),
                "chat.message".to_string(),
                "event".to_string()
            ],
            "hook names registered in declaration order"
        );

        // trigger mutates output in place (v1 semantics)
        let out = sc
            .trigger(
                "tool.execute.after",
                json!({"tool": "write", "args": {"filePath": "/x"}}),
                json!({}),
            )
            .await
            .expect("trigger");
        assert_eq!(
            out["seen"], "write",
            "hook observed input and mutated output"
        );
        // unregistered hook name → output unchanged
        let out2 = sc
            .trigger("permission.ask", json!({}), json!({"keep": 1}))
            .await
            .expect("trigger2");
        assert_eq!(out2["keep"], 1);

        // v1 event delivery (plugin/index.ts:255-259): hooks["event"]
        // receives {event:{id,type,properties}}; shape-asserting fixture
        // only succeeds if the wrapper is exact (delivered counts non-
        // throwing hooks), and a throwing event hook fails open.
        let ev = sc
            .emit_event(json!({"id": "e1", "type": "session.created", "properties": {}}))
            .await
            .expect("emit_event");
        assert_eq!(ev["delivered"], 1, "event hook must receive exact shape");
        let ev2 = sc
            .emit_event(json!({"id": "e2", "type": "boom.type", "properties": {}}))
            .await
            .expect("emit_event fail-open");
        assert_eq!(ev2["delivered"], 0, "throwing event hook must not count");

        let st = sc.statuses();
        assert_eq!(st["synthetic"]["ok"], true);
        assert_eq!(st["synthetic"]["hooks"][0], "tool.execute.after");

        sc.shutdown().await;
        Ok(())
    }

    #[tokio::test]
    async fn sidecar_respawns_after_death_and_reloads_plugins() -> anyhow::Result<()> {
        if !runtime_available() {
            panic!(
                "plugin-runtime binary ({}) required for plugin-host tests (M4b environment gate)",
                crate::plugin_runtime().program()
            );
        }
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("plug.mjs"),
            r#"
export const PluginModule = {
  id: "reaper",
  server: async () => ({
    "chat.message": async (_i, output) => { output.alive = true; },
  }),
};
export default PluginModule.server;
"#,
        )
        .unwrap();
        let host = materialize_host(&dir.path().join("host"))?;
        let mut sc = Sidecar::spawn(&host, "http://127.0.0.1:9", "/work").await?;
        sc.load(
            "reaper",
            &dir.path().join("plug.mjs"),
            &json!({}),
            &Value::Null,
        )
        .await
        .expect("load");
        let pid = sc.child_pid();
        // force-kill the child (the live OOM class: sidecar dies, hooks
        // silently broken-piped until a respawn guard exists)
        std::process::Command::new("kill")
            .arg("-9")
            .arg(pid.to_string())
            .status()
            .expect("kill -9");
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;

        // next RPC must respawn + replay loads + deliver the mutation
        let out = sc
            .trigger("chat.message", json!({"sessionID": "s"}), json!({}))
            .await
            .expect("trigger after respawn");
        assert_eq!(out["alive"], true, "hook must fire after respawn");
        assert_ne!(sc.child_pid(), pid, "child process must be replaced");
        let st = sc.statuses();
        assert_eq!(
            st["reaper"]["ok"], true,
            "previously loaded plugins must be reloaded"
        );
        sc.shutdown().await;
        Ok(())
    }

    /// v2 plugin battery (Phase 3): both dialects through the real sidecar.
    /// A promise-flavor plugin registers a session hook + an RPC; an
    /// effect-flavor plugin (with its own `effect` runtime vendored beside it)
    /// registers a tool hook. Asserts detection, dispatch, RPC round-trip, the
    /// real inventory, and that unregistered/missing RPCs keep exact errors.
    #[tokio::test]
    async fn v2_battery_promise_and_effect_dialects() -> anyhow::Result<()> {
        if !runtime_available() {
            panic!(
                "plugin-runtime binary ({}) required for plugin-host tests",
                crate::plugin_runtime().program()
            );
        }
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("p_v2_promise.mjs"),
            r#"export default {
  id: "v2promise",
  setup: async (ctx) => {
    await ctx.session.hook("context", (ev) => { ev.promiseTouched = true; });
    await ctx.tool.hook("execute.after", (ev) => { ev.result = "mutated-by-promise"; });
    await ctx.rpc.register({ id: "sample" }, {
      hello: async (input) => ({ echoed: input, from: "promise" }),
    });
    await ctx.storage.set("k", 41);
    if ((await ctx.storage.get("k")) !== 41) throw new Error("storage broke");
  },
};"#,
        )
        .unwrap();
        let eff = dir
            .path()
            .join("p_v2_effect")
            .join("node_modules")
            .join("effect");
        std::fs::create_dir_all(eff.join("lib")).unwrap();
        std::fs::write(
            eff.join("package.json"),
            r#"{"name":"effect","version":"3.0.0","type":"module","exports":{".":"./lib/index.js"}}"#,
        )
        .unwrap();
        std::fs::write(
            eff.join("lib/index.js"),
            "export const Effect = { scoped: (p) => p, runPromise: async (p) => { await p; } };\nexport const Scope = {};\n",
        )
        .unwrap();
        std::fs::write(
            dir.path().join("p_v2_effect").join("plug.mjs"),
            r#"export default {
  id: "v2effect",
  effect: (ctx) => (async () => {
    await ctx.tool.hook("execute.before", (ev) => { ev.input.before = "effect"; });
  })(),
};"#,
        )
        .unwrap();

        let host = materialize_host(&dir.path().join("host"))?;
        let mut sc = Sidecar::spawn(&host, "http://127.0.0.1:9", "/work").await?;
        let input = json!({"directory": "/work", "serverUrl": "http://127.0.0.1:9"});

        let h1 = sc
            .load(
                "v2promise",
                &dir.path().join("p_v2_promise.mjs"),
                &input,
                &Value::Null,
            )
            .await?;
        assert!(
            h1.contains(&"session.context".to_string()),
            "promise hooks: {h1:?}"
        );
        assert!(h1.contains(&"tool.execute.after".to_string()));
        let h2 = sc
            .load(
                "v2effect",
                &dir.path().join("p_v2_effect").join("plug.mjs"),
                &input,
                &Value::Null,
            )
            .await?;
        assert!(
            h2.contains(&"tool.execute.before".to_string()),
            "effect hooks: {h2:?}"
        );

        let out = sc.trigger_v2("session", "context", json!({})).await?;
        assert_eq!(out["matched"], 1);
        assert_eq!(out["output"]["promiseTouched"], true);
        let after = sc
            .trigger_v2(
                "tool",
                "execute.after",
                json!({"tool": "read", "result": "orig"}),
            )
            .await?;
        assert_eq!(after["output"]["result"], "mutated-by-promise");
        let before = sc
            .trigger_v2(
                "tool",
                "execute.before",
                json!({"tool": "read", "input": {}}),
            )
            .await?;
        assert_eq!(before["output"]["input"]["before"], "effect");

        let rpc = sc.rpc_call("sample", "hello", json!({"n": 7})).await?;
        assert_eq!(rpc["output"]["echoed"]["n"], 7);
        let miss = sc.rpc_call("sample", "nope", json!({})).await?;
        assert_eq!(miss["error"]["type"], "rpc.method_not_found");
        let unreg = sc.rpc_call("ghost", "hello", json!({})).await?;
        assert_eq!(unreg["error"]["type"], "rpc.unavailable");

        let inv = sc.v2_inventory().await?;
        assert_eq!(inv["plugins"].as_array().unwrap().len(), 2, "{inv}");
        assert!(
            inv["rpcs"]
                .as_array()
                .unwrap()
                .iter()
                .any(|r| r == "sample")
        );

        sc.shutdown().await;
        Ok(())
    }

    /// A v2 plugin that registers a transform on an UNBRIDGED domain must still
    /// load, and the divergence must be reported.
    ///
    /// The corpus run on 2026-10-21 found four real plugins failing to load with
    /// `D-V2-DOMAIN: ctx.<domain>.transform is not bridged`. They were not
    /// broken plugins: they register transforms during `setup`, which is
    /// upstream's only way to contribute to those domains, and our shim threw on
    /// the registration call. That failed the WHOLE plugin, discarding the hooks
    /// we do bridge — `session.context`, `tool.execute.before`, events. A
    /// divergence must degrade one capability, never the plugin.
    ///
    /// The transform is accepted and recorded, never invoked (calling it would
    /// mean applying an edit ocserve cannot apply), and the domain is reported in
    /// the inventory so the operator can see it.
    #[tokio::test]
    async fn v2_unbridged_transform_registers_instead_of_failing_setup() -> anyhow::Result<()> {
        if !runtime_available() {
            panic!(
                "plugin-runtime binary ({}) required for plugin-host tests",
                crate::plugin_runtime().program()
            );
        }
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("p_v2_edit.mjs"),
            r#"export default {
  id: "v2edit",
  setup: async (ctx) => {
    // A transform on a domain ocserve does not bridge (upstream's registration
    // form, so this is what real plugins do).
    await ctx.tool.transform((editor) => { editor.remove("bash"); });
    await ctx.integration.transform(() => {});
    // The common transform -> reload -> list sequence: reload must resolve.
    await ctx.provider.reload();
    // The hooks we DO bridge must survive.
    await ctx.session.hook("context", (ev) => { ev.survived = true; });
  },
};"#,
        )
        .unwrap();

        let host = materialize_host(&dir.path().join("host"))?;
        let mut sc = Sidecar::spawn(&host, "http://127.0.0.1:9", "/work").await?;
        let input = json!({"directory": "/work", "serverUrl": "http://127.0.0.1:9"});

        let hooks = sc
            .load(
                "v2edit",
                &dir.path().join("p_v2_edit.mjs"),
                &input,
                &Value::Null,
            )
            .await
            .expect("a transform on an unbridged domain must not fail setup");

        // The bridged hook registered by the same plugin still works.
        assert!(
            hooks.contains(&"session.context".to_string()),
            "the bridged hook must survive: {hooks:?}"
        );
        let out = sc.trigger_v2("session", "context", json!({})).await?;
        assert_eq!(out["matched"], 1);
        assert_eq!(out["output"]["survived"], true);

        // And the unbridged domains are reported, not silently swallowed.
        let inv = sc.v2_inventory().await?;
        let unbridged: Vec<String> = inv["unbridged_transforms"]
            .as_array()
            .expect("inventory must list unbridged transforms")
            .iter()
            .map(|v| v.as_str().unwrap_or_default().to_string())
            .collect();
        assert!(unbridged.contains(&"tool".to_string()), "{inv}");
        assert!(unbridged.contains(&"integration".to_string()), "{inv}");

        // Genuine USE of an unbridged domain still throws its named divergence
        // (the capability really is absent; only registration is tolerated).
        std::fs::write(
            dir.path().join("p_v2_use.mjs"),
            r#"export default {
  id: "v2use",
  setup: async (ctx) => { await ctx.model.list(); },
};"#,
        )
        .unwrap();
        let err = sc
            .load(
                "v2use",
                &dir.path().join("p_v2_use.mjs"),
                &input,
                &Value::Null,
            )
            .await
            .expect_err("using an unbridged domain must still fail loudly");
        let text = format!("{err:#}");
        assert!(text.contains("D-V2-DOMAIN"), "{text}");
        assert!(text.contains("model"), "{text}");

        sc.shutdown().await;
        Ok(())
    }

    /// The v2 REST half of a domain is bound to the route that actually exists —
    /// and an UNBOUND verb keeps the named divergence instead of a bare TypeError.
    ///
    /// The 2026-10-18 finding: `session: { ...registrar("session") }` gave
    /// `ctx.session` only `hook`, so `ctx.session.get(id)` was
    /// `TypeError: ... is not a function` — a crash, not the honest throw the rest
    /// of the shim uses. `ctx.plugin.list()` likewise returned `[]` while
    /// `/api/plugin` serves the real inventory (D-PLUGIN-INVENTORY was closed).
    #[tokio::test]
    async fn v2_domain_rest_methods_bind_real_routes() -> anyhow::Result<()> {
        if !runtime_available() {
            panic!(
                "plugin-runtime binary ({}) required for plugin-host tests",
                crate::plugin_runtime().program()
            );
        }
        // A real HTTP responder so ctx.session.get() exercises an actual route
        // (stdlib thread; the shim fetches it like any other client would).
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut s) = stream else { continue };
                use std::io::{Read, Write};
                let mut buf = [0u8; 2048];
                let _ = s.read(&mut buf);
                let body = br#"{"id":"ses_bound","title":"bound-route"}"#;
                let _ = s.write_all(
                format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                    body.len()
                )
                .as_bytes(),
            );
                let _ = s.write_all(body);
            }
        });

        let dir = tempfile::tempdir().unwrap();
        // The fixture reports each probe's outcome back over RPC so the test can
        // read real results (not shim internals).
        std::fs::write(
            dir.path().join("rest.mjs"),
            r#"export default {
  id: "restprobe",
  setup: async (ctx) => {
    const results = {};
    // plugin.list must return the REAL inventory, not [].
    results.pluginList = await ctx.plugin.list();
    // session.get must reach the bound route.
    try {
      const s = await ctx.session.get("ses_bound");
      results.sessionGet = { ok: true, id: s && s.id };
    } catch (e) { results.sessionGet = { ok: false, err: String(e && e.message || e) }; }
    // An UNBOUND verb must throw the NAMED divergence, not a TypeError.
    try { await ctx.session.notARealVerb("x"); results.unbound = { threw: false }; }
    catch (e) { results.unbound = { threw: true, msg: String(e && e.message || e) }; }
    await ctx.rpc.register({ id: "probe" }, { results: async () => results });
  },
};"#,
        )
        .unwrap();

        let host = materialize_host(&dir.path().join("host"))?;
        let mut sc = Sidecar::spawn(&host, &format!("http://127.0.0.1:{port}"), "/work").await?;
        let input = json!({"directory": "/work", "serverUrl": format!("http://127.0.0.1:{port}")});
        sc.load(
            "restprobe",
            &dir.path().join("rest.mjs"),
            &input,
            &Value::Null,
        )
        .await?;

        let r = sc.rpc_call("probe", "results", json!({})).await?;
        let out = &r["output"];

        // 1. plugin.list is real (contains our own id), not empty.
        let listed = out["pluginList"].as_array().cloned().unwrap_or_default();
        assert!(
            listed.iter().any(|p| p["id"] == "restprobe"),
            "ctx.plugin.list() must report the loaded plugin: {out}"
        );

        // 2. session.get hit the bound route (the stub answered ses_bound).
        assert_eq!(
            out["sessionGet"]["ok"], true,
            "ctx.session.get must reach /api/session/{{id}}: {out}"
        );
        assert_eq!(out["sessionGet"]["id"], "ses_bound");

        // 3. an unbound verb throws the NAMED divergence (D-V2-DOMAIN), not a
        //    TypeError and not silently succeeding.
        assert_eq!(
            out["unbound"]["threw"], true,
            "unbound verb must throw: {out}"
        );
        let msg = out["unbound"]["msg"].as_str().unwrap_or_default();
        assert!(
            msg.contains("D-V2-DOMAIN"),
            "unbound verb must name the divergence (D-V2-DOMAIN), got: {msg}"
        );

        sc.shutdown().await;
        Ok(())
    }

    /// A v2 plugin's `command.transform` / `skill.transform` edits are PULLED by
    /// Rust and come back as the list both consumers serve.
    ///
    /// The all-consumers rule (TESTING §1): `skill_entries()` filters
    /// `Payloads.command` on `source=="skill"`, so a skill transform must change
    /// the SYSTEM PROMPT the model reads, not just the `/command` route. A test
    /// that only asserted the RPC round-trip would pass while the prompt kept the
    /// old text — shape without mutation.
    ///
    /// Also covers: add REPLACES a same-named entry (upstream `editor.set`,
    /// core/command.ts:61, so a reload is idempotent), and a THROWING transform is
    /// reported without taking the list down.
    #[tokio::test]
    async fn v2_command_transform_pulls_into_the_served_list() -> anyhow::Result<()> {
        if !runtime_available() {
            panic!(
                "plugin-runtime binary ({}) required for plugin-host tests",
                crate::plugin_runtime().program()
            );
        }
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("tx.mjs"),
            r#"export default {
  id: "tx",
  setup: async (ctx) => {
    // upstream shape: ctx.command.transform((editor) => editor.add({...}))
    await ctx.command.transform((editor) => {
      editor.add({ name: "plugin-cmd", description: "added by plugin" });
    });
    await ctx.skill.transform((editor) => {
      editor.update("demo-skill", (s) => { s.description = "rewritten by plugin"; });
      editor.add({ name: "plugin-skill", description: "skill from plugin", source: "skill" });
    });
    await ctx.command.reload();
    const listed = await ctx.command.list();
    await ctx.rpc.register({ id: "probe" }, {
      afterSetup: async () => ({ listed }),
    });
  },
};"#,
        )
        .unwrap();

        let host = materialize_host(&dir.path().join("host"))?;
        let mut sc = Sidecar::spawn(&host, "http://127.0.0.1:1", "/work").await?;
        let input = json!({"directory": "/work"});
        sc.load("tx", &dir.path().join("tx.mjs"), &input, &Value::Null)
            .await?;

        // The pristine list: one skill (what ocserve's own config produced) and
        // one config command. Skills and commands share ONE vector.
        let pristine = vec![
            json!({"name": "demo-skill", "description": "original", "source": "skill"}),
            json!({"name": "review", "description": "config command", "source": "config"}),
        ];
        let (edited, failures) = sc.transform_commands(pristine.clone()).await?;

        assert!(
            failures.is_empty(),
            "no transform should have failed: {failures:?}"
        );
        // 1. the command transform's add landed
        let added = edited
            .iter()
            .find(|c| c["name"] == "plugin-cmd")
            .expect("plugin command must be present");
        assert_eq!(added["description"], "added by plugin");
        // 2. the skill transform's update landed — THIS is the prompt-visible half
        let skill = edited
            .iter()
            .find(|c| c["name"] == "demo-skill")
            .expect("demo-skill must still be present");
        assert_eq!(
            skill["description"], "rewritten by plugin",
            "skill.transform must reach the vector skill_entries() reads"
        );
        // 3. the skill add landed with the skill source (so skill_entries picks it up)
        assert!(
            edited
                .iter()
                .any(|c| c["name"] == "plugin-skill" && c["source"] == "skill"),
            "plugin skill must carry source=skill or skill_entries() drops it: {edited:?}"
        );
        // 4. untouched entries survive (the all-consumers rule cuts both ways)
        assert!(edited.iter().any(|c| c["name"] == "review"));

        // 5. IDEMPOTENT: a second pull from pristine yields the same list (add
        //    replaces, never accumulates).
        let (again, _) = sc.transform_commands(pristine.clone()).await?;
        assert_eq!(
            serde_json::to_value(&again).unwrap(),
            serde_json::to_value(&edited).unwrap(),
            "a reload must not accumulate duplicates"
        );

        // 6. a pull over a one-entry list keeps that entry (the plugin's own adds are
        //    additive; the caller must never receive a shorter list than it sent)
        let (kept, _) = sc
            .transform_commands(vec![json!({"name": "keepme"})])
            .await?;
        assert!(kept.iter().any(|c| c["name"] == "keepme"));

        sc.shutdown().await;
        Ok(())
    }

    /// A throwing v2 transform is reported, never fatal — upstream's state machine
    /// disables a failing transform (core/state.ts:178-184) rather than
    /// propagating, because a broken third-party plugin must not take the server's
    /// command surface down with it.
    #[tokio::test]
    async fn v2_command_transform_failure_keeps_the_list() -> anyhow::Result<()> {
        if !runtime_available() {
            panic!(
                "plugin-runtime binary ({}) required for plugin-host tests",
                crate::plugin_runtime().program()
            );
        }
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("boom.mjs"),
            r#"export default {
  id: "boom",
  setup: async (ctx) => {
    await ctx.command.transform(() => { throw new Error("transform exploded"); });
    await ctx.command.transform((editor) => { editor.add({ name: "survivor" }); });
  },
};"#,
        )
        .unwrap();

        let host = materialize_host(&dir.path().join("host"))?;
        let mut sc = Sidecar::spawn(&host, "http://127.0.0.1:1", "/work").await?;
        let input = json!({"directory": "/work"});
        sc.load("boom", &dir.path().join("boom.mjs"), &input, &Value::Null)
            .await?;

        let (kept, failures) = sc
            .transform_commands(vec![json!({"name": "existing"})])
            .await?;
        // the good transform still ran (a broken one does not abort the rest)
        assert!(
            kept.iter().any(|c| c["name"] == "survivor"),
            "a throwing transform must not stop the others: {kept:?}"
        );
        assert!(kept.iter().any(|c| c["name"] == "existing"));
        // and the failure is reported so the server can log + count it
        assert_eq!(failures.len(), 1, "the throwing transform must be reported");
        assert!(
            failures[0]["message"]
                .as_str()
                .unwrap_or_default()
                .contains("transform exploded"),
            "{failures:?}"
        );

        sc.shutdown().await;
        Ok(())
    }

    /// The `config` hook is dispatched (v1 parity, plugin/index.ts:244-251).
    /// Closes the registered-but-never-called class: a plugin whose `config`
    /// is a real hook must see it run (host `config` used to hard-code "ok").
    #[tokio::test]
    async fn config_hook_is_dispatched() -> anyhow::Result<()> {
        if !runtime_available() {
            panic!(
                "plugin-runtime binary ({}) required for plugin-host tests",
                crate::plugin_runtime().program()
            );
        }
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("cfg.mjs"),
            r#"export const PluginModule = {
  id: "cfg",
  server: async () => ({
    config: async (cfg) => {
      if (typeof cfg !== "object" || cfg === null) throw new Error("config not object");
    },
    "chat.message": async () => {},
  }),
};
export default PluginModule.server;"#,
        )
        .unwrap();
        let host = materialize_host(&dir.path().join("host"))?;
        let mut sc = Sidecar::spawn(&host, "http://127.0.0.1:9", "/work").await?;
        sc.load("cfg", &dir.path().join("cfg.mjs"), &json!({}), &Value::Null)
            .await?;
        let r = sc.config(json!({"model": "x"})).await?;
        assert_eq!(r["matched"], 1, "config hook must be dispatched: {r}");
        sc.shutdown().await;
        Ok(())
    }

    /// Test environment gate: the ACTUAL runtime (bun preferred) must exist.
    fn runtime_available() -> bool {
        crate::which(crate::plugin_runtime().program()).is_some()
    }

    /// Real-plugin battery (V2-EXECUTION-PLAN Phase 2): every v1 authoring
    /// form the frozen client supports (plugin/index.ts readV1Plugin +
    /// getLegacyPlugins) plus the `[spec, options]` tuple. Loads all forms
    /// through ONE sidecar (as production does) and asserts each registered
    /// the expected hook AND that options actually reach the factory (the
    /// silently-dropped-tuple bug this battery exists to pin).
    #[tokio::test]
    async fn battery_all_authoring_forms_and_options() -> anyhow::Result<()> {
        if !runtime_available() {
            panic!(
                "plugin-runtime binary ({}) required for plugin-host tests",
                crate::plugin_runtime().program()
            );
        }
        let dir = tempfile::tempdir().unwrap();
        // 1. PluginModule.server (v1 canonical)
        std::fs::write(
            dir.path().join("p_module.mjs"),
            r#"export const PluginModule = {
  id: "m",
  server: async () => ({ "chat.message": async (_i, o) => { o.module = true; } }),
};"#,
        )
        .unwrap();
        // 2. default object with .server
        std::fs::write(
            dir.path().join("p_default_obj.mjs"),
            r#"export default { server: async () => ({ "chat.params": async (_i, o) => { o.defaultobj = true; } }) };"#,
        )
        .unwrap();
        // 3. default function (legacy)
        std::fs::write(
            dir.path().join("p_default_fn.mjs"),
            r#"export default async () => ({ "tool.execute.after": async (_i, o) => { o.defaultfn = true; } });"#,
        )
        .unwrap();
        // 4. named plain-function export (each IS a server factory)
        std::fs::write(
            dir.path().join("p_named.mjs"),
            r#"export async function MyPlugin() { return { "event": async () => {} }; }"#,
        )
        .unwrap();
        // 5. options tuple: the factory must SEE the second tuple element.
        std::fs::write(
            dir.path().join("p_options.mjs"),
            r#"export const PluginModule = {
  id: "opts",
  server: async (_input, options) => ({
    "chat.message": async (_i, o) => { o.sawOptions = options; },
  }),
};"#,
        )
        .unwrap();

        let host = materialize_host(&dir.path().join("host"))?;
        let mut sc = Sidecar::spawn(&host, "http://127.0.0.1:9", "/work").await?;
        let input =
            json!({"directory": "/work", "projectID": "global", "serverUrl": "http://127.0.0.1:9"});

        let h1 = sc
            .load("m", &dir.path().join("p_module.mjs"), &input, &Value::Null)
            .await?;
        assert_eq!(h1, vec!["chat.message"], "PluginModule.server form");
        let h2 = sc
            .load(
                "dobj",
                &dir.path().join("p_default_obj.mjs"),
                &input,
                &Value::Null,
            )
            .await?;
        assert_eq!(h2, vec!["chat.params"], "default object with .server form");
        let h3 = sc
            .load(
                "dfn",
                &dir.path().join("p_default_fn.mjs"),
                &input,
                &Value::Null,
            )
            .await?;
        assert_eq!(
            h3,
            vec!["tool.execute.after"],
            "default function (legacy) form"
        );
        let h4 = sc
            .load(
                "named",
                &dir.path().join("p_named.mjs"),
                &input,
                &Value::Null,
            )
            .await?;
        assert_eq!(h4, vec!["event"], "named function export form");

        let opts = json!({"token": "abc", "n": 3});
        let h5 = sc
            .load("opts", &dir.path().join("p_options.mjs"), &input, &opts)
            .await?;
        assert_eq!(
            h5,
            vec!["chat.message"],
            "options-tuple form registers same hook"
        );

        // Dispatch across ALL loaded plugins: each form's hook fires.
        let out = sc.trigger("chat.message", json!({}), json!({})).await?;
        assert_eq!(out["module"], true, "form 1 hook fired");
        // options reached the factory (the tuple was NOT dropped)
        assert_eq!(out["sawOptions"], opts, "factory saw the tuple options");
        let out2 = sc.trigger("chat.params", json!({}), json!({})).await?;
        assert_eq!(out2["defaultobj"], true, "form 2 hook fired");
        let out3 = sc
            .trigger("tool.execute.after", json!({}), json!({}))
            .await?;
        assert_eq!(out3["defaultfn"], true, "form 3 hook fired");

        // statuses carry all five loads (real inventory, not empty).
        let st = sc.statuses();
        for spec in ["m", "dobj", "dfn", "named", "opts"] {
            assert_eq!(st[spec]["ok"], true, "status for {spec}");
        }
        sc.shutdown().await;
        Ok(())
    }
}

#[cfg(test)]
mod runtime_selection_tests {
    /// `OCSERVE_PLUGIN_RUNTIME` forces the runtime (lab A/B control);
    /// unset/unknown → bun-if-present else node. Env is process-local and
    /// restored (this file is the only env mutator in its own process).
    #[test]
    fn runtime_override_and_default_preference() {
        let has_bun = crate::which("bun").is_some();
        let has_node = crate::which("node").is_some();
        if !has_node {
            panic!("node required for the fallback leg (test environment gate)");
        }

        // SAFETY: single-threaded env mutation, restored below.
        unsafe { std::env::set_var("OCSERVE_PLUGIN_RUNTIME", "node") };
        assert_eq!(crate::plugin_runtime(), crate::PluginRuntime::Node);

        if has_bun {
            unsafe { std::env::set_var("OCSERVE_PLUGIN_RUNTIME", "bun") };
            assert_eq!(crate::plugin_runtime(), crate::PluginRuntime::Bun);
            unsafe { std::env::remove_var("OCSERVE_PLUGIN_RUNTIME") };
            assert_eq!(
                crate::plugin_runtime(),
                crate::PluginRuntime::Bun,
                "bun present + no override → bun preferred (upstream fidelity)"
            );
        } else {
            unsafe { std::env::remove_var("OCSERVE_PLUGIN_RUNTIME") };
            assert_eq!(crate::plugin_runtime(), crate::PluginRuntime::Node);
        }

        // unknown value falls through to preference, never panics
        unsafe { std::env::set_var("OCSERVE_PLUGIN_RUNTIME", "deno") };
        let _ = crate::plugin_runtime();
        unsafe { std::env::remove_var("OCSERVE_PLUGIN_RUNTIME") };
    }
}

#[cfg(test)]
mod oom_wrapper_tests {
    /// A3 proof: the spawn wrapper makes the CHILD report adj=500 so a
    /// memcg OOM kill takes the child (respawnable) — not the main service.
    #[cfg(target_os = "linux")]
    #[test]
    fn wrapper_raises_child_oom_score_adj() {
        let out = std::process::Command::new("sh")
            .arg("-c")
            .arg(ocserve_store::cgroup::WRAPPER)
            .arg("sh")
            .arg("cat")
            .arg("/proc/self/oom_score_adj")
            .output()
            .expect("wrapper spawns");
        assert!(
            out.status.success(),
            "wrapper failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert_eq!(
            String::from_utf8_lossy(&out.stdout).trim(),
            "500",
            "child must run at adj=500"
        );
    }
}
