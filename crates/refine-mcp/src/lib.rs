//! refine-mcp: MCP client (stdio NDJSON + StreamableHTTP) with the wire
//! behaviors verified live against context7 (§1152): protocol 2025-06-18,
//! `text/event-stream` responses parsed for `data:` lines, stateless remote
//! (no session header required), 30s request timeout (upstream DEFAULT_TIMEOUT),
//! tools/list cursor pagination bounded at 1000 pages (catalog.ts MAX_LIST_PAGES).
//!
//! Tool naming = upstream catalog.toolName: sanitize(server)_sanitize(tool)
//! where sanitize = [^a-zA-Z0-9_-] → `_`.

use anyhow::{Context, Result, anyhow};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::process::Stdio;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

/// Upstream catalog.ts sanitize: non [a-zA-Z0-9_-] → `_`.
pub fn sanitize(value: &str) -> String {
    value
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

/// Upstream catalog.ts toolName.
pub fn tool_name(server: &str, tool: &str) -> String {
    format!("{}_{}", sanitize(server), sanitize(tool))
}

/// Request timeout (catalog.ts DEFAULT_TIMEOUT = 30s).
pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
/// NDJSON line cap (MEMORY §6 parse cap).
pub const MAX_LINE_BYTES: usize = 8 * 1024 * 1024;
/// tools/list pagination bound (catalog.ts MAX_LIST_PAGES).
pub const MAX_LIST_PAGES: usize = 1_000;
pub const PROTOCOL_VERSION: &str = "2025-06-18";

#[derive(Debug, Clone)]
pub enum Kind {
    Local {
        command: Vec<String>,
        env: Vec<(String, String)>,
    },
    Remote {
        url: String,
        headers: Vec<(String, String)>,
    },
}

#[derive(Debug, Clone)]
pub struct ServerCfg {
    pub name: String,
    pub enabled: bool,
    pub kind: Kind,
}

/// Parse opencode config `mcp` object: {name: {type, command?, environment?,
/// url?, headers?, enabled?}} (shape verified from live config).
pub fn parse_config(mcp: &Value) -> Vec<ServerCfg> {
    let Some(obj) = mcp.as_object() else {
        return Vec::new();
    };
    obj.iter()
        .map(|(name, v)| {
            let enabled = v.get("enabled").and_then(|e| e.as_bool()).unwrap_or(true);
            let ty = v.get("type").and_then(|t| t.as_str()).unwrap_or("local");
            let kind = if ty == "remote" {
                Kind::Remote {
                    url: v
                        .get("url")
                        .and_then(|u| u.as_str())
                        .unwrap_or_default()
                        .to_string(),
                    headers: v
                        .get("headers")
                        .and_then(|h| h.as_object())
                        .map(|h| {
                            h.iter()
                                .map(|(k, val)| {
                                    (k.clone(), val.as_str().unwrap_or_default().to_string())
                                })
                                .collect()
                        })
                        .unwrap_or_default(),
                }
            } else {
                Kind::Local {
                    command: v
                        .get("command")
                        .and_then(|c| c.as_array())
                        .map(|a| {
                            a.iter()
                                .filter_map(|s| s.as_str().map(String::from))
                                .collect()
                        })
                        .unwrap_or_default(),
                    env: v
                        .get("environment")
                        .and_then(|e| e.as_object())
                        .map(|e| {
                            e.iter()
                                .map(|(k, val)| {
                                    (k.clone(), val.as_str().unwrap_or_default().to_string())
                                })
                                .collect()
                        })
                        .unwrap_or_default(),
                }
            };
            ServerCfg {
                name: name.clone(),
                enabled,
                kind,
            }
        })
        .collect()
}

enum Transport {
    /// boxed (clippy large_enum_variant): Child is much larger than the HTTP arm
    Stdio(Box<StdioTransport>),
    Http {
        http: reqwest::Client,
        url: String,
        headers: Vec<(String, String)>,
        next_id: i64,
    },
}

struct StdioTransport {
    child: tokio::process::Child,
    stdin: tokio::process::ChildStdin,
    reader: BufReader<tokio::process::ChildStdout>,
    next_id: i64,
}

pub struct McpClient {
    pub name: String,
    transport: Transport,
}

/// MCP spawn PATH (W2): systemd's PATH has no user tool dirs — boot log
/// showed `spawn context-mode: No such file`. Prepend standard user
/// locations that exist and aren't already in PATH; never drop entries.
pub fn mcp_child_path(parent_path: &str, home: &str) -> String {
    let candidates = [
        format!("{home}/.local/bin"),
        format!("{home}/bin"),
        format!("{home}/.bun/bin"), // bun global bins (context-mode)
        "/home/linuxbrew/.linuxbrew/bin".to_string(),
    ];
    let existing: Vec<&str> = parent_path.split(':').collect();
    let mut merged: Vec<String> = candidates
        .into_iter()
        .filter(|c| std::path::Path::new(c).is_dir() && !existing.contains(&c.as_str()))
        .collect();
    if !parent_path.is_empty() {
        merged.push(parent_path.to_string());
    }
    merged.join(":")
}

impl McpClient {
    /// Spawn (local) or prepare (remote). Errors are transport-level only;
    /// initialize() performs the handshake.
    pub async fn connect(cfg: &ServerCfg) -> Result<Self> {
        match &cfg.kind {
            Kind::Local { command, env } => {
                let Some((prog, rest)) = command.split_first() else {
                    anyhow::bail!("empty command for mcp server {}", cfg.name);
                };
                let mut cmd = tokio::process::Command::new(prog);
                let home = std::env::var("HOME").unwrap_or_else(|_| "/".into());
                let base_path = std::env::var("PATH").unwrap_or_default();
                cmd.args(rest)
                    .env("PATH", mcp_child_path(&base_path, &home))
                    .envs(env.iter().cloned())
                    .stdin(Stdio::piped())
                    .stdout(Stdio::piped())
                    .stderr(Stdio::null())
                    .kill_on_drop(true);
                let mut child = cmd.spawn().with_context(|| format!("spawn {}", prog))?;
                let stdin = child.stdin.take().context("child stdin")?;
                let stdout = child.stdout.take().context("child stdout")?;
                Ok(Self {
                    name: cfg.name.clone(),
                    transport: Transport::Stdio(Box::new(StdioTransport {
                        child,
                        stdin,
                        reader: BufReader::new(stdout),
                        next_id: 1,
                    })),
                })
            }
            Kind::Remote { url, headers } => {
                let http = reqwest::Client::builder()
                    .timeout(REQUEST_TIMEOUT)
                    .build()?;
                Ok(Self {
                    name: cfg.name.clone(),
                    transport: Transport::Http {
                        http,
                        url: url.clone(),
                        headers: headers.clone(),
                        next_id: 1,
                    },
                })
            }
        }
    }

    async fn request(&mut self, method: &str, params: Value) -> Result<Value> {
        match &mut self.transport {
            Transport::Stdio(t) => {
                let StdioTransport {
                    stdin,
                    reader,
                    next_id,
                    ..
                } = t.as_mut();
                let id = *next_id;
                *next_id += 1;
                let msg = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
                let mut line = msg.to_string();
                line.push('\n');
                let fut = async {
                    stdin.write_all(line.as_bytes()).await?;
                    stdin.flush().await?;
                    // read until the response with our id (skip notifications)
                    loop {
                        let mut buf = String::new();
                        let n = reader.read_line(&mut buf).await?;
                        if n == 0 {
                            return Err(anyhow!("mcp stdout closed waiting for {method}"));
                        }
                        if buf.len() > MAX_LINE_BYTES {
                            return Err(anyhow!("mcp line exceeds {MAX_LINE_BYTES} cap"));
                        }
                        let v: Value = serde_json::from_str(&buf)
                            .with_context(|| format!("mcp line parse for {method}"))?;
                        if v.get("id").and_then(|i| i.as_i64()) == Some(id) {
                            if let Some(err) = v.get("error") {
                                return Err(anyhow!("mcp error: {err}"));
                            }
                            return Ok(v.get("result").cloned().unwrap_or(Value::Null));
                        }
                        // notification or unrelated id → skip
                    }
                };
                tokio::time::timeout(REQUEST_TIMEOUT, fut)
                    .await
                    .map_err(|_| anyhow!("mcp {method} timed out after {REQUEST_TIMEOUT:?}"))?
            }
            Transport::Http {
                http,
                url,
                headers,
                next_id,
            } => {
                let id = *next_id;
                *next_id += 1;
                let body = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
                let mut req = http
                    .post(url.as_str())
                    .header("content-type", "application/json")
                    .header("accept", "application/json, text/event-stream")
                    .body(body.to_string());
                for (k, v) in headers.iter() {
                    req = req.header(k.as_str(), v.as_str());
                }
                let resp = req.send().await.context("mcp http send")?;
                let status = resp.status();
                if !status.is_success() {
                    let body = resp.text().await.unwrap_or_default();
                    return Err(anyhow!(
                        "mcp http {status}: {}",
                        &body[..body.len().min(200)]
                    ));
                }
                let ct = resp
                    .headers()
                    .get(reqwest::header::CONTENT_TYPE)
                    .and_then(|v| v.to_str().ok())
                    .unwrap_or_default()
                    .to_string();
                let raw = resp.bytes().await?;
                if raw.len() > MAX_LINE_BYTES {
                    return Err(anyhow!("mcp response exceeds {MAX_LINE_BYTES} cap"));
                }
                let text = String::from_utf8_lossy(&raw);
                let v: Value = if ct.contains("event-stream") {
                    // `data:` lines (live-verified: event: message\ndata: {...})
                    let mut parsed = None;
                    for line in text.lines() {
                        if let Some(d) = line.strip_prefix("data:")
                            && let Ok(v) = serde_json::from_str::<Value>(d.trim())
                        {
                            parsed = Some(v);
                            break;
                        }
                    }
                    parsed.ok_or_else(|| anyhow!("mcp SSE response had no data line"))?
                } else {
                    serde_json::from_str(&text).context("mcp http json parse")?
                };
                if let Some(err) = v.get("error") {
                    return Err(anyhow!("mcp error: {err}"));
                }
                Ok(v.get("result").cloned().unwrap_or(Value::Null))
            }
        }
    }

    async fn notify(&mut self, method: &str, params: Value) -> Result<()> {
        match &mut self.transport {
            Transport::Stdio(t) => {
                let stdin = &mut t.stdin;
                let msg = json!({"jsonrpc": "2.0", "method": method, "params": params});
                let mut line = msg.to_string();
                line.push('\n');
                stdin.write_all(line.as_bytes()).await?;
                stdin.flush().await?;
                Ok(())
            }
            Transport::Http { .. } => Ok(()), // stateless remote: notifications optional
        }
    }

    /// Handshake: initialize + notifications/initialized.
    pub async fn initialize(&mut self) -> Result<Value> {
        let result = self
            .request(
                "initialize",
                json!({
                    "protocolVersion": PROTOCOL_VERSION,
                    "capabilities": {},
                    "clientInfo": {"name": "refine", "version": env!("CARGO_PKG_VERSION")}
                }),
            )
            .await?;
        self.notify("notifications/initialized", json!({})).await?;
        Ok(result)
    }

    /// tools/list with cursor pagination (bounded).
    pub async fn list_tools(&mut self) -> Result<Vec<Value>> {
        let mut out = Vec::new();
        let mut cursor: Option<String> = None;
        for _ in 0..MAX_LIST_PAGES {
            let params = match &cursor {
                Some(c) => json!({"cursor": c}),
                None => json!({}),
            };
            let result = self.request("tools/list", params).await?;
            if let Some(tools) = result.get("tools").and_then(|t| t.as_array()) {
                out.extend(tools.iter().cloned());
            }
            let next = result
                .get("nextCursor")
                .and_then(|c| c.as_str())
                .map(String::from);
            if next.is_none() {
                return Ok(out);
            }
            cursor = next;
        }
        Err(anyhow!("mcp list exceeded {MAX_LIST_PAGES} pages"))
    }

    /// tools/call → joined text content (upstream convertTool text-join).
    pub async fn call_tool(&mut self, name: &str, args: Value) -> Result<String> {
        let result = self
            .request("tools/call", json!({"name": name, "arguments": args}))
            .await?;
        if result
            .get("isError")
            .and_then(|b| b.as_bool())
            .unwrap_or(false)
        {
            let msg = text_of(&result);
            return Err(anyhow!(
                "{}",
                if msg.is_empty() {
                    "MCP tool returned an error".to_string()
                } else {
                    msg
                }
            ));
        }
        Ok(text_of(&result))
    }

    pub async fn close(&mut self) {
        if let Transport::Stdio(t) = &mut self.transport {
            let _ = t.child.kill().await;
        }
    }
}

fn text_of(result: &Value) -> String {
    result["content"]
        .as_array()
        .map(|items| {
            items
                .iter()
                .filter(|i| i["type"] == "text")
                .filter_map(|i| i["text"].as_str())
                .filter(|t| !t.trim().is_empty())
                .collect::<Vec<_>>()
                .join("\n\n")
        })
        .unwrap_or_default()
}

pub mod trust;

/// per-server trust state (connect-time scan + TOFU pin) — additive on /mcp
#[derive(Clone, Debug)]
pub struct ServerTrust {
    pub verdict: String,
    pub findings: Vec<String>,
    pub pin: String,
    pub drifted: bool,
}

/// Connected clients + probe statuses (serve-lifetime hub).
#[derive(Default)]
pub struct McpHub {
    clients: parking_lot::Mutex<HashMap<String, McpClient>>,
    trust: parking_lot::Mutex<HashMap<String, ServerTrust>>,
    /// last probe: name → ("connected" | "failed", error)
    statuses: parking_lot::Mutex<HashMap<String, (String, Option<String>)>>,
    /// lazy tool-schema cache (populated on first prompt; listChanged=false)
    tools_cache: parking_lot::Mutex<Option<Vec<Value>>>,
    /// enabled configs retained so connect can respawn a disconnected server
    cfgs: parking_lot::Mutex<Vec<ServerCfg>>,
}

impl McpHub {
    /// Probe all configured servers (initialize handshake), record statuses.
    /// Failures are captured, never fatal (upstream: status map, §1146).
    pub async fn probe_all(cfgs: &[ServerCfg]) -> McpHub {
        let hub = McpHub::default();
        *hub.cfgs.lock() = cfgs.iter().filter(|c| c.enabled).cloned().collect();
        for cfg in cfgs.iter().filter(|c| c.enabled) {
            match McpClient::connect(cfg).await {
                Ok(mut client) => match client.initialize().await {
                    Ok(_) => {
                        hub.statuses
                            .lock()
                            .insert(cfg.name.clone(), ("connected".into(), None));
                        hub.clients.lock().insert(cfg.name.clone(), client);
                    }
                    Err(e) => {
                        hub.statuses
                            .lock()
                            .insert(cfg.name.clone(), ("failed".into(), Some(format!("{e:#}"))));
                        client.close().await;
                    }
                },
                Err(e) => {
                    hub.statuses
                        .lock()
                        .insert(cfg.name.clone(), ("failed".into(), Some(format!("{e:#}"))));
                }
            }
        }
        hub
    }

    /// W5: is the server name configured (enabled)?
    pub fn known(&self, name: &str) -> bool {
        self.cfgs.lock().iter().any(|c| c.name == name)
    }

    /// W5: POST /mcp/{name}/disconnect — drop+kill the client, mark status.
    pub async fn disconnect(&self, name: &str) -> anyhow::Result<()> {
        let client = self.clients.lock().remove(name);
        match client {
            Some(mut c) => {
                c.close().await;
                self.statuses
                    .lock()
                    .insert(name.to_string(), ("disconnected".into(), None));
                *self.tools_cache.lock() = None; // schemas refetch on next prompt
                Ok(())
            }
            None => anyhow::bail!("MCP server not found: {name}"),
        }
    }

    /// W5: POST /mcp/{name}/connect — (re)spawn + handshake.
    pub async fn connect(&self, name: &str) -> anyhow::Result<()> {
        let cfg = self
            .cfgs
            .lock()
            .iter()
            .find(|c| c.name == name)
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("MCP server not found: {name}"))?;
        // bind BEFORE the await: if-let scrutinee temporaries (parking_lot
        // guard) live through the body — held across .await = !Send future
        let old = self.clients.lock().remove(name);
        if let Some(mut old) = old {
            old.close().await;
        }
        match McpClient::connect(&cfg).await {
            Ok(mut client) => match client.initialize().await {
                Ok(_) => {
                    self.statuses
                        .lock()
                        .insert(name.to_string(), ("connected".into(), None));
                    self.clients.lock().insert(name.to_string(), client);
                    *self.tools_cache.lock() = None;
                    Ok(())
                }
                Err(e) => {
                    let msg = format!("{e:#}");
                    client.close().await;
                    self.statuses
                        .lock()
                        .insert(name.to_string(), ("failed".into(), Some(msg.clone())));
                    anyhow::bail!(msg)
                }
            },
            Err(e) => {
                let msg = format!("{e:#}");
                self.statuses
                    .lock()
                    .insert(name.to_string(), ("failed".into(), Some(msg.clone())));
                Err(e)
            }
        }
    }

    /// Freeze route shape: {name: {status, error?}} (§1146).
    pub fn statuses(&self) -> Value {
        let mut out = serde_json::Map::new();
        for (name, (status, err)) in self.statuses.lock().iter() {
            let mut v = serde_json::Map::new();
            v.insert("status".into(), json!(status));
            if let Some(e) = err {
                v.insert("error".into(), json!(e));
            }
            // additive trust object (D3) — client tolerance probed:
            // oc-remote Json { ignoreUnknownKeys, coerceInputValues } = true
            if let Some(t) = self.trust.lock().get(name) {
                v.insert(
                    "trust".into(),
                    json!({
                        "verdict": t.verdict,
                        "findings": t.findings,
                        "pin": t.pin,
                        "drifted": t.drifted,
                    }),
                );
            }
            out.insert(name.clone(), Value::Object(v));
        }
        Value::Object(out)
    }

    /// Runtime observe-only scan of an MCP tool's response (the unguarded
    /// channel OWASP names: connect-time review never sees responses).
    /// 64 KiB bounded; metric + log only — never blocks, never mutates.
    pub fn observe_output(tool: &str, output: &str) {
        let bounded = output.get(..65536).unwrap_or(output);
        let hits = trust::scan_text(bounded);
        if !hits.is_empty() {
            refine_metrics::labeled_counter(
                "refine_mcp_tool_flagged_total",
                &format!("tool=\"{tool}\""),
                1,
            );
            tracing::warn!(tool, findings = ?hits, "mcp tool output flagged (observe-only)");
        }
    }

    /// Take a client out of the hub for an await (guards must not cross
    /// await — parking_lot MutexGuard is !Send; axum needs Send futures).
    fn take(&self, name: &str) -> Option<McpClient> {
        self.clients.lock().remove(name)
    }

    fn put(&self, client: McpClient) {
        self.clients.lock().insert(client.name.clone(), client);
    }

    /// All connected servers' tools as provider-function schemas, named with
    /// the upstream toolName scheme (sanitize(server)_sanitize(tool)).
    /// Cached after first successful listing (context7 advertises
    /// listChanged=false; refresh hook lands when a server says otherwise).
    pub async fn tool_schemas(&self) -> Vec<Value> {
        if let Some(cached) = self.tools_cache.lock().clone() {
            return cached;
        }
        let names: Vec<String> = self.clients.lock().keys().cloned().collect();
        let mut out = Vec::new();
        for name in names {
            let Some(mut client) = self.take(&name) else {
                continue;
            };
            let listed = client.list_tools().await;
            self.put(client);
            match listed {
                Ok(tools) => {
                    let mut surface: Vec<(String, String, String)> = Vec::new();
                    for t in &tools {
                        let Some(tname) = t.get("name").and_then(|n| n.as_str()) else {
                            continue;
                        };
                        let params = t
                            .get("inputSchema")
                            .cloned()
                            .unwrap_or_else(|| json!({"type": "object", "properties": {}}));
                        let desc = t.get("description").and_then(|d| d.as_str()).unwrap_or("");
                        // connect-time static scan (OWASP MCP03 metadata channel)
                        let hits = trust::scan_tool(tname, desc, &params.to_string());
                        for h in hits {
                            let hs = h.to_string();
                            surface.push((
                                format!("{tname}::{h}"),
                                "flagged".into(),
                                String::new(),
                            ));
                            let _ = hs;
                        }
                        surface.push((tname.to_string(), desc.to_string(), params.to_string()));
                        out.push(json!({
                            "type": "function",
                            "function": {
                                "name": tool_name(&name, tname),
                                "description": desc,
                                "parameters": params,
                            }
                        }));
                    }
                    // TOFU pin + verdict (recomputed on every (re)list — the
                    // drift compare is what catches a mid-process rug-pull)
                    let pins: Vec<(String, String, String)> = surface
                        .iter()
                        .filter(|(_, d, _)| d != "flagged")
                        .cloned()
                        .collect();
                    let mut findings: Vec<String> = surface
                        .iter()
                        .filter(|(_, d, _)| d == "flagged")
                        .map(|(n, _, _)| n.clone())
                        .collect();
                    findings.sort();
                    findings.dedup();
                    let pin = trust::pin_tools(&pins);
                    let mut drifted = false;
                    {
                        let mut map = self.trust.lock();
                        if let Some(prev) = map.get(&name)
                            && prev.pin != pin
                        {
                            drifted = true;
                            refine_metrics::labeled_counter(
                                "refine_mcp_tool_drift_total",
                                &format!("server=\"{name}\""),
                                1,
                            );
                            tracing::warn!(
                                server = %name,
                                old = %prev.pin,
                                new = %pin,
                                "mcp tool surface drifted since first observation (TOFU)"
                            );
                        }
                        let v = trust::verdict(&findings);
                        refine_metrics::labeled_counter(
                            "refine_mcp_trust_total",
                            &format!("server=\"{name}\",verdict=\"{v}\""),
                            1,
                        );
                        map.insert(
                            name.clone(),
                            ServerTrust {
                                verdict: v.to_string(),
                                findings,
                                pin,
                                drifted,
                            },
                        );
                    }
                }
                Err(e) => {
                    tracing::warn!("mcp {name} tools/list failed: {e:#}");
                }
            }
        }
        *self.tools_cache.lock() = Some(out.clone());
        out
    }

    /// Call a namespaced MCP tool: `server_tool` → (server, tool).
    /// Returns Ok(None) if the name is not an MCP tool (caller tries builtins).
    pub async fn call(&self, namespaced: &str, args: Value) -> Option<Result<String>> {
        let candidates: Vec<String> = self.clients.lock().keys().cloned().collect();
        for server in candidates {
            let prefix = format!("{}_", sanitize(&server));
            let Some(rest) = namespaced.strip_prefix(&prefix) else {
                continue;
            };
            let mut client = self.take(&server)?;
            let result = client
                .call_tool(rest, args)
                .await
                .with_context(|| format!("mcp {server}/{rest}"));
            self.put(client);
            return Some(result);
        }
        None
    }
}

#[cfg(test)]
mod path_tests {
    use super::mcp_child_path;

    #[test]
    fn prepends_existing_home_dirs_once_and_keeps_parent() {
        let home = std::env::temp_dir();
        std::fs::create_dir_all(home.join(".local/bin")).unwrap();
        let parent = "/usr/bin:/bin";
        let merged = mcp_child_path(parent, home.to_str().unwrap());
        assert!(merged.starts_with(&format!("{}", home.join(".local/bin").display())));
        assert!(merged.ends_with(parent), "parent PATH preserved: {merged}");
        // idempotent: already-present candidate is not duplicated
        let merged2 = mcp_child_path(&merged, home.to_str().unwrap());
        assert_eq!(merged, merged2, "no duplicate entries");
        // non-existent home subdirs are skipped
        let merged3 = mcp_child_path(parent, "/nonexistent-home-xyz");
        assert!(
            !merged3.contains("nonexistent-home-xyz") && merged3.ends_with(parent),
            "no phantom dirs from a missing home: {merged3}"
        );
        // empty parent tolerated
        let merged4 = mcp_child_path("", home.to_str().unwrap());
        assert!(merged4.contains(".local/bin"));
    }
}

#[cfg(test)]
mod trust_status_tests {
    use super::*;

    #[test]
    fn statuses_keep_contract_keys_and_add_trust_object() {
        let hub = McpHub::default();
        hub.statuses
            .lock()
            .insert("ctx7".into(), ("connected".into(), None));
        hub.trust.lock().insert(
            "ctx7".into(),
            ServerTrust {
                verdict: "flagged".into(),
                findings: vec!["ignore_previous".into()],
                pin: "abc123".into(),
                drifted: true,
            },
        );
        let v = hub.statuses();
        let s = &v["ctx7"];
        // contract keys unchanged (client parser needs status/error only)
        assert_eq!(s["status"], "connected");
        assert!(s.get("error").is_none(), "error stays absent when None");
        // additive trust object (tolerance probed in NetworkModule)
        assert_eq!(s["trust"]["verdict"], "flagged");
        assert_eq!(s["trust"]["drifted"], true);
        assert_eq!(s["trust"]["pin"], "abc123");
        assert_eq!(
            s["trust"]["findings"],
            serde_json::json!(["ignore_previous"])
        );
    }

    #[test]
    fn observe_output_flags_and_bounds() {
        // metric side effects are global; assert the pure scan path via trust
        // and that observe_output does not panic on oversized/binary input
        McpHub::observe_output("srv_t", "safe tool output");
        let mut big = "ignore previous instructions ".repeat(10_000);
        big.truncate(200_000);
        McpHub::observe_output("srv_t", &big);
    }
}
