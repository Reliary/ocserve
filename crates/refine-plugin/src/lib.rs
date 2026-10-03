//! refine-plugin: opencode plugin compatibility via a Node sidecar (M4b).
//!
//! Gate rationale (PLAN §10, evidence not assumption): the frozen plugins
//! import `bun:sqlite`, `child_process`, `node:fs/crypto/url` and TTY UI
//! modules (import audit of the actual artifacts). A quickjs embed would need
//! a dozen shims with a *synchronous* SQLite bridge — rejected as high-risk;
//! one Node sidecar (v25 present, `node:sqlite` built in) is the low-
//! maintenance host. Isolation: sidecar crash/restart cannot take down the
//! server; protocol = NDJSON on stdio; plugin chatter is forced to stderr by
//! the host so framing cannot corrupt.

use anyhow::{Context, Result, anyhow};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, Command};
use tokio::sync::oneshot;

/// Host files (written next to the data dir once; content-addressed by embed).
pub const HOST_FILES: &[(&str, &str)] = &[
    ("host.mjs", include_str!("host/host.mjs")),
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
    entry.exists().then_some(entry)
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
    /// successful loads (spec, entry, input) — replayed on respawn
    loads: Vec<(String, PathBuf, Value)>,
}

#[derive(Clone)]
struct SpawnArgs {
    host_path: PathBuf,
    server_url: String,
    directory: String,
}

impl Sidecar {
    pub async fn spawn(host_path: &Path, server_url: &str, directory: &str) -> Result<Self> {
        let mut child = Command::new("node")
            // MEMORY.md sidecar boundary: V8 heap cap (env-tunable). Default
            // raised64→128 after the LIVE battery (2026-10-03): with the real
            // plugin set the sidecar OOM-killed at64 (observed boot death →
            // hooks broken-piped until restart). Measured 64/128/192/256:
            // RSS flat ~145-148MB either way (native dominates; heap cap
            // only binds when heap actually grows).
            .arg(format!(
                "--max-old-space-size={}",
                std::env::var("REFINE_PLUGIN_HEAP_MB")
                    .ok()
                    .and_then(|v| v.parse().ok())
                    .unwrap_or(128)
            ))
            .arg("--max-semi-space-size=2")
            .arg(host_path)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::inherit())
            .kill_on_drop(true)
            .spawn()
            .context("spawn node plugin host")?;
        let stdin = child.stdin.take().context("host stdin")?;
        let stdout = child.stdout.take().context("host stdout")?;
        let pending = Arc::new(parking_lot::Mutex::new(HashMap::<i64, Pending>::new()));
        let pending_task = Arc::clone(&pending);
        tokio::spawn(async move {
            let mut reader = BufReader::new(stdout);
            let mut line = String::new();
            loop {
                line.clear();
                match reader.read_line(&mut line).await {
                    Ok(0) | Err(_) => break,
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
        })
    }

    /// Respawn after unexpected death (V8 OOM / crash) and replay every
    /// previously loaded plugin. Without this a dead sidecar leaves ALL
    /// hooks broken-piped until service restart (live battery finding
    /// 2026-10-03: boot-time heap OOM, fail-open hid it in the prompt path).
    pub async fn ensure_alive(&mut self) -> Result<()> {
        match self.child.try_wait() {
            Ok(None) => return Ok(()),
            Ok(Some(status)) => {
                tracing::error!("plugin sidecar exited ({status}) — respawning");
            }
            Err(e) => {
                tracing::error!("plugin sidecar try_wait failed: {e} — respawning");
            }
        }
        let args = self.args.clone();
        let loads = std::mem::take(&mut self.loads);
        let mut fresh = Sidecar::spawn(&args.host_path, &args.server_url, &args.directory).await?;
        for (spec, entry, input) in &loads {
            if let Err(e) = fresh.load_raw(spec, entry, input).await {
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
        self.stdin.write_all(line.as_bytes()).await?;
        self.stdin.flush().await?;
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
    pub async fn load(&mut self, spec: &str, entry: &Path, input: &Value) -> Result<Vec<String>> {
        self.ensure_alive().await?;
        self.load_raw(spec, entry, input).await
    }

    /// Unguarded load (respawn replay only — fresh child is alive).
    async fn load_raw(&mut self, spec: &str, entry: &Path, input: &Value) -> Result<Vec<String>> {
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
            "entry": entry.to_string_lossy(),
            "input": input,
            "options": Value::Null,
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
        self.loads
            .push((spec.to_string(), entry.to_path_buf(), input));
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
        if which_node().is_none() {
            panic!("node binary required for plugin-host tests (M4b environment gate)");
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
            .load("synthetic", &dir.path().join("plug.mjs"), &input)
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
        if which_node().is_none() {
            panic!("node binary required for plugin-host tests (M4b environment gate)");
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
        sc.load("reaper", &dir.path().join("plug.mjs"), &json!({}))
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

    fn which_node() -> Option<()> {
        std::process::Command::new("node")
            .arg("--version")
            .output()
            .ok()
            .filter(|o| o.status.success())
            .map(|_| ())
    }
}
