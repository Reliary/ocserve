//! Sync prompt runner: POST /session/{id}/message semantics (captured live,
//! testdata/m2/session_fixture.json + prompt_response.json + tool_fixture).
//!
//! M2b: multi-step loop — stream → tool calls → permission gate → execute →
//! persist tool parts → next turn → final message. All assistants parent the
//! user message (fixture fact). Durable events carry sync twins (per-session
//! seq, seeded once).

use crate::event::{EventBus, frame, sync_frame};
use crate::ids::{msg_id, prt_id};
use crate::permission::PermissionGate;
use anyhow::{Context, Result};
use futures_util::StreamExt;
use ocserve_llm::{ChatMessage, Client, StreamEvent, ToolCallAssembler, Usage};
use ocserve_store::{BlobStore, insert_message};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

#[derive(Clone)]
pub struct LlmEndpoint {
    pub base_url: String,
    pub api_key: String,
    /// (input, output, cache_read) USD per MTok — None → cost 0.
    pub pricing: Option<(f64, f64, f64)>,
}

#[derive(Clone)]
pub struct PromptContext {
    pub db: PathBuf,
    pub blobs: Arc<BlobStore>,
    pub bus: EventBus,
    pub directory: String,
    /// Session default agent (payload.agent overrides per prompt).
    pub agent: String,
    pub system: String,
    pub endpoint: LlmEndpoint,
    pub model_id: String,
    pub provider_id: String,
    /// Agent permission rules (v1 {permission, pattern, action}, last wins).
    pub rules: Vec<ocserve_tools::Rule>,
    /// Permission rendezvous (ask → event → reply).
    pub gate: Arc<PermissionGate>,
    /// MCP hub (M4a): namespaced tools merged into the provider tool list.
    pub mcp: Option<Arc<ocserve_mcp::McpHub>>,
    /// Plugin sidecar (M4b): hook dispatch (v1 names, e.g. tool.execute.after).
    pub plugins: Option<Arc<tokio::sync::Mutex<ocserve_plugin::Sidecar>>>,
    /// Question rendezvous (v1 Question service): `question` tool gate.
    pub questions: Arc<crate::question::QuestionGate>,
    /// model limit block from the catalog (M6: compaction trigger math).
    pub model_limit: serde_json::Value,
    /// K-AUTONOMY: hard round cap for this run (0 = unlimited). Resolved
    /// ONCE from env at AppState construction (sound for parallel tests;
    /// changing it needs a restart, like provider_stall).
    pub max_rounds: usize,
    /// K-AUTONOMY: hard USD ceiling for this run (0.0 = off).
    pub cost_ceiling: f64,
    /// compaction config (shared opencode.json `compaction` section).
    pub compaction: crate::compact::CompactionCfg,
    /// system prompt for compaction requests (hidden `compaction` agent).
    pub compaction_system: String,
}

pub(crate) fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Project a single provider step's `Usage` into the assistant message
/// `tokens` shape (upstream `Session.getUsage`, session.ts:368-377):
/// `input` EXCLUDES cache reads/writes (billed separately); `output` excludes
/// reasoning; `total` is the provider's raw `total_tokens`. This is the ONLY
/// shape persisted on assistant messages and step-finish parts — a whole-turn
/// sum here inflated the last message and the web UI's context meter.
pub(crate) fn message_tokens(u: &Usage) -> Value {
    json!({
        "total": u.total_tokens,
        "input": u.prompt_tokens.saturating_sub(u.cached_tokens),
        "output": u.completion_tokens.saturating_sub(u.reasoning_tokens),
        "reasoning": u.reasoning_tokens,
        "cache": {"write": 0, "read": u.cached_tokens},
    })
}

/// Emit a durable event: persist to ring, publish plain frame + sync twin.
/// `seq` is a session-local counter seeded once (avoids re-opening readers).
pub(crate) fn emit_durable(
    ctx: &PromptContext,
    writer: &ocserve_store::Writer,
    session_id: &str,
    event_type: &str,
    properties: Value,
    seq: &mut i64,
) -> Result<()> {
    // normalize ONCE so the persisted payload and every published frame agree
    // (the event-log validator reads the persisted copy).
    let properties = crate::event::normalize_props(event_type, properties);
    ocserve_store::append_event(writer, Some(session_id), event_type, &properties)?;
    ocserve_metrics::labeled_counter(
        "ocserve_events_emitted_total",
        &format!("type=\"{event_type}\""),
        1,
    );
    let this_seq = *seq;
    *seq += 1;
    ctx.bus
        .publish(frame(&ctx.directory, event_type, properties.clone()));
    ctx.bus.publish(sync_frame(
        &ctx.directory,
        event_type,
        properties,
        this_seq as u64,
        session_id,
    ));
    Ok(())
}

/// Emit `session.updated` with the FULL session merged with `overrides`
/// (upstream `patch()`: `{...current, ...info}`). The event MUST carry a
/// complete Session — a partial `info` merges into client stores and crashes
/// renderers that read `title` (TUI `r.title.length`, 2026-10-09). `overrides`
/// carries mid-prompt values not yet persisted (cost/tokens land at finalize).
pub(crate) fn emit_session_updated(
    ctx: &PromptContext,
    writer: &ocserve_store::Writer,
    session_id: &str,
    overrides: Value,
    seq: &mut i64,
) -> Result<()> {
    let info = match ocserve_store::session_wire_with(&ctx.db, session_id, overrides) {
        Ok(Some(full)) => full,
        // Session row missing (deleted mid-turn): emit the partial rather than
        // nothing — a `session.deleted` should be the authoritative signal, and
        // a shape guard would flag a missing title if this path is ever hit.
        Ok(None) => json!({"id": session_id}),
        Err(e) => {
            tracing::warn!("session_wire_with({session_id}): {e:#}");
            json!({"id": session_id})
        }
    };
    emit_durable(
        ctx,
        writer,
        session_id,
        "session.updated",
        json!({"sessionID": session_id, "info": info}),
        seq,
    )
}

/// Loop-guard permission rendezvous (D1/P1b): same durable event shape as
/// the normal permission ask but `action="doom_loop"` (upstream's permission
/// name, processor.ts:373) with additive metadata.class. Returns true when
/// the user approved once/always.
#[allow(clippy::too_many_arguments)]
async fn loop_guard_ask(
    ctx: &PromptContext,
    writer: &ocserve_store::Writer,
    session_id: &str,
    class: crate::loop_guard::Class,
    tool: &str,
    input_key: &str,
    message_id: &str,
    call_id: &str,
    seq: &mut i64,
) -> Result<bool> {
    let perm_id = crate::ids::per_id();
    // exact shape the live normal ask emits (permission/patterns/always/
    // metadata/tool) — oc-remote's parser consumes this shape proven M2b;
    // upstream's permission name for the doom case is `doom_loop`
    let request = json!({
        "id": perm_id,
        "sessionID": session_id,
        "permission": "doom_loop",
        "patterns": [tool],
        "always": [tool],
        "metadata": {"class": class.as_str(), "tool": tool, "input": input_key},
        "tool": {"messageID": message_id, "callID": call_id},
    });
    let (rx, _perm_guard) = ctx.gate.clone().register(&perm_id, request.clone());
    emit_durable(ctx, writer, session_id, "permission.asked", request, seq)?;
    let reply = ctx.gate.wait(&perm_id, rx).await;
    emit_durable(
        ctx,
        writer,
        session_id,
        "permission.replied",
        // spec: {sessionID, requestID, reply} — reply is required
        json!({"sessionID": session_id, "requestID": perm_id, "reply": reply}),
        seq,
    )?;
    if reply == "always" {
        ctx.gate.grant_always(session_id, "doom_loop", tool);
        // K-ALWAYS: persist past restart (memory stays the fast path)
        if let Err(e) =
            ocserve_store::session_grant_always(writer, &ctx.db, session_id, "doom_loop", tool)
        {
            tracing::error!("persist always grant doom_loop:{tool}: {e:#}");
        }
    }
    Ok(reply == "once" || reply == "always")
}

/// Trigger a plugin hook with v1 in-place mutation semantics: returns the
/// (possibly mutated) `output`. Fail-open by construction — no sidecar or a
/// hook error returns `output` unchanged (a broken plugin never breaks a
/// prompt; M4b rule). Metrics: duration + error counter per hook name.
pub async fn hook_mutate(
    plugins: Option<&Arc<tokio::sync::Mutex<ocserve_plugin::Sidecar>>>,
    name: &str,
    input: Value,
    output: Value,
) -> Value {
    let Some(plug) = plugins else {
        return output;
    };
    let mut guard = plug.lock().await;
    let t0 = std::time::Instant::now();
    match guard.trigger(name, input, output.clone()).await {
        Ok(v) => {
            ocserve_metrics::observe(
                "ocserve_plugin_hook_duration_seconds",
                &format!("hook=\"{name}\",result=\"ok\""),
                t0.elapsed().as_micros() as u64,
            );
            v
        }
        Err(e) => {
            ocserve_metrics::observe(
                "ocserve_plugin_hook_duration_seconds",
                &format!("hook=\"{name}\",result=\"error\""),
                t0.elapsed().as_micros() as u64,
            );
            ocserve_metrics::labeled_counter(
                "ocserve_plugin_hook_errors_total",
                &format!("hook=\"{name}\""),
                1,
            );
            tracing::warn!("plugin {name}: {e:#}");
            output
        }
    }
}

/// Emit a non-durable live event (delta/status/idle/diff — no sync twin, per capture).
pub(crate) fn emit_live(ctx: &PromptContext, event_type: &str, properties: Value) {
    ocserve_metrics::labeled_counter(
        "ocserve_events_emitted_total",
        &format!("type=\"{event_type}\""),
        1,
    );
    ctx.bus
        .publish(frame(&ctx.directory, event_type, properties));
}

/// Build the assistant message skeleton emitted at the START of a provider turn
/// (upstream prompt.ts:1186-1201 `sessions.updateMessage(msg)` before the
/// processor runs). Required keys are all present; `time.completed` is
/// deliberately absent so clients render the message as streaming
/// (`streaming = !time.completed`, app bundle). The same `MessageID` is reused
/// when the message is persisted at turn end so stream and persisted copy
/// reconcile to ONE message. The same skeleton feeds the client via
/// `message.updated`.
pub(crate) fn assistant_message_start(
    ctx: &PromptContext,
    session_id: &str,
    parent_id: &str,
    assistant_id: &str,
    agent: &str,
    model: &str,
    created: i64,
) -> Value {
    json!({
        "id": assistant_id,
        "sessionID": session_id,
        "role": "assistant",
        "parentID": parent_id,
        "mode": "primary",
        "agent": agent,
        "path": {"cwd": ctx.directory, "root": "/"},
        "cost": 0,
        "tokens": {"input": 0, "output": 0, "reasoning": 0,
                   "cache": {"read": 0, "write": 0}},
        "modelID": model,
        "providerID": ctx.provider_id,
        "time": {"created": created},
    })
}

/// Emit a start part (empty text) before deltas, so the client's delta reducer
/// finds the part it accumulates into. Upstream emits `updatePart` at
/// `text-start`/`reasoning-start` (processor.ts:280-291,500-511) before any
/// `updatePartDelta`. Without this, `message.part.delta` is silently dropped
/// (the 2026-10-11 "web UI doesn't update until reload" bug).
pub(crate) fn emit_part_start(
    ctx: &PromptContext,
    session_id: &str,
    assistant_id: &str,
    part_id: &str,
    part_type: &str,
    started: i64,
) {
    emit_live(
        ctx,
        "message.part.updated",
        json!({"sessionID": session_id, "part": {
            "type": part_type, "id": part_id, "text": "",
            "sessionID": session_id, "messageID": assistant_id,
            "time": {"start": started},
        }}),
    );
}

/// Reconstruct provider messages from stored history (text + tool parts).
/// History byte budget (K-AUTONOMY scale hole, MEMORY §7.3): the live prompt
/// build was UNBOUNDED by session size — giant sessions × concurrent prompts
/// = the anon risk at thousands-of-sessions scale. Tail-weighted like
/// compaction's D2/COMPACTION_CONVERSATION_MAX_BYTES: drop OLDEST first,
/// always keep at least the newest exchange. Under-budget sessions are
/// byte-identical (no wire/behavior change; the common case). Dropped
/// rounds are counted (metric) so silent context loss is measurable.
pub(crate) const PROMPT_HISTORY_MAX_BYTES: usize = 8 * 1024 * 1024;

fn history_message_bytes(info: &Value, parts: &[Value]) -> usize {
    let mut n = 64; // per-message envelope slack
    for p in parts {
        n += 96;
        for k in ["text", "arguments"] {
            if let Some(s) = p.get(k).and_then(|v| v.as_str()) {
                n += s.len();
            }
        }
        if let Some(s) = p.pointer("/state/output").and_then(|v| v.as_str()) {
            n += s.len();
        }
        if let Some(s) = p.pointer("/state/input").and_then(|v| v.as_str()) {
            n += s.len();
        }
    }
    n += info.to_string().len().min(2048);
    n
}

pub(crate) fn to_provider_messages(history: &[(Value, Vec<Value>)]) -> Vec<ChatMessage> {
    // tail-weighted budget: find the oldest message we can afford to keep
    let mut total: usize = history
        .iter()
        .map(|(i, p)| history_message_bytes(i, p))
        .sum();
    let mut start = 0usize;
    while total > PROMPT_HISTORY_MAX_BYTES && start + 1 < history.len() {
        let (i, p) = &history[start];
        total -= history_message_bytes(i, p);
        start += 1;
    }
    if start > 0 {
        ocserve_metrics::labeled_counter(
            "ocserve_history_truncated_total",
            &format!("dropped=\"{}\"", start),
            1,
        );
        tracing::warn!(
            "prompt history budget: dropped {start}/{} oldest message(s) (>{} bytes)",
            history.len(),
            PROMPT_HISTORY_MAX_BYTES
        );
    }
    let mut out = Vec::new();
    for (info, parts) in &history[start..] {
        let role = info["role"].as_str().unwrap_or("user");
        let text: String = parts
            .iter()
            .filter(|p| p["type"] == "text")
            .filter_map(|p| p["text"].as_str())
            .collect();
        let tool_parts: Vec<&Value> = parts.iter().filter(|p| p["type"] == "tool").collect();
        if tool_parts.is_empty() {
            if !text.is_empty() || role == "user" {
                out.push(ChatMessage::text(role, text));
            }
            continue;
        }
        // assistant with tool calls: declare calls, then emit results
        let calls: Vec<Value> = tool_parts
            .iter()
            .map(|p| {
                json!({
                    "id": p["callID"],
                    "type": "function",
                    "function": {
                        "name": p["tool"],
                        "arguments": p["state"]["input"].to_string(),
                    }
                })
            })
            .collect();
        out.push(ChatMessage::assistant_with_tools(text, calls));
        for p in &tool_parts {
            let output = p["state"]["output"]
                .as_str()
                .or_else(|| p["state"]["metadata"]["output"].as_str())
                .unwrap_or_default();
            // provider-boundary sift (P1c): history rebuild must emit the
            // SAME bytes as the live site below or the prefix cache busts
            let bound =
                crate::sift_boundary::maybe_sift(p["tool"].as_str().unwrap_or_default(), output);
            out.push(ChatMessage::tool_result(
                p["callID"].as_str().unwrap_or_default(),
                bound.to_string(),
            ));
        }
    }
    out
}

/// Run one prompt to completion (possibly multi-step via tools).
/// Returns the FINAL assistant (info, parts) — the HTTP response body.
/// Provider stall budget (A3 watchdog). Env-overridable ONLY so integration
/// tests can shrink it in their own process (tests/stall.rs sets
/// OCSERVE_PROVIDER_STALL_SECS=1); production default 120s — deltas keep
/// resetting it, so only a truly silent socket trips it.
pub(crate) fn provider_stall() -> std::time::Duration {
    static S: std::sync::OnceLock<std::time::Duration> = std::sync::OnceLock::new();
    *S.get_or_init(|| {
        std::time::Duration::from_secs(
            std::env::var("OCSERVE_PROVIDER_STALL_SECS")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(120),
        )
    })
}

/// K-EFFICIENCY: per-prompt RSS + reader-open accounting. Emission lives in
/// Drop so bail paths (cap, provider errors, `?`) report too — tail-only
/// emission was proven wrong by the cap e2e going red.
struct PhaseProbe {
    rss0: Option<i64>,
    peak: Option<i64>,
    opens0: u64,
}

impl PhaseProbe {
    fn start() -> Self {
        Self {
            rss0: ocserve_metrics::rss_bytes(),
            peak: ocserve_metrics::rss_bytes(),
            opens0: ocserve_store::pragma::reader_opens(),
        }
    }
    fn sample_round(&mut self) {
        if let Some(now) = ocserve_metrics::rss_bytes() {
            self.peak = Some(self.peak.map_or(now, |p: i64| p.max(now)));
        }
    }
}

impl Drop for PhaseProbe {
    fn drop(&mut self) {
        ocserve_metrics::counter(
            "ocserve_db_opens_total",
            ocserve_store::pragma::reader_opens().saturating_sub(self.opens0),
        );
        if let (Some(start), Some(peak)) = (self.rss0, self.peak) {
            ocserve_metrics::gauge("ocserve_prompt_rss_start_bytes", start);
            ocserve_metrics::gauge("ocserve_prompt_rss_delta_bytes", (peak - start).max(0));
        }
    }
}

/// Run a builtin tool on the blocking pool (K-EFFICIENCY: never pin an
/// async worker with sync process I/O).
async fn spawn_tool(
    name: &str,
    input: Value,
    directory: &str,
) -> Result<ocserve_tools::ToolResult> {
    let name = name.to_string();
    let dir = directory.to_string();
    tokio::task::spawn_blocking(move || ocserve_tools::execute(&name, &input, Path::new(&dir)))
        .await
        .map_err(|e| anyhow::anyhow!("tool join: {e}"))?
}

/// Execution knobs for `run_prompt_with`. Defaults = every existing caller's
/// behavior (prompt/message/command/shell/abort paths unchanged).
#[derive(Clone)]
pub struct RunOpts {
    /// persist the incoming payload's user message (false: summarize runs a
    /// transient instruction — only the assistant reply lands in history)
    pub persist_user: bool,
    /// skip loading session history as provider messages (prelude carries it)
    pub skip_history: bool,
    /// extra user message for the MODEL when skip_history (summarize prompt)
    pub prelude: Option<String>,
    /// send tools to the provider (false: summarization is text-only)
    pub tools_enabled: bool,
    /// hard cap on provider↔tool rounds this run (None → env
    /// OCSERVE_PROMPT_MAX_ROUNDS; 0/absent = unlimited — K-AUTONOMY:
    /// sessions must run as long as they need, including overnight)
    pub max_rounds: Option<usize>,
    /// hard USD ceiling for this run (None → env OCSERVE_PROMPT_MAX_COST_USD;
    /// 0.0/absent = off — the bound on DOLLARS, not rounds)
    pub cost_ceiling: Option<f64>,
    /// auto-title default-titled sessions from the first user message
    /// (false: command/shell flows must not name sessions after a command)
    pub auto_title: bool,
}

impl Default for RunOpts {
    fn default() -> Self {
        // NOTE: derive(Default) would make every bool false — persist_user /
        // tools_enabled MUST default true or existing callers silently change
        // (caught immediately: prompt_async/command persistence tests red).
        Self {
            persist_user: true,
            skip_history: false,
            prelude: None,
            tools_enabled: true,
            max_rounds: None,
            cost_ceiling: None,
            auto_title: true,
        }
    }
}

pub async fn run_prompt(
    ctx: &PromptContext,
    writer: &ocserve_store::Writer,
    session_id: &str,
    req: &crate::wire::PromptRequest,
) -> Result<(Value, Vec<Value>)> {
    let res = run_prompt_with(ctx, writer, session_id, req, RunOpts::default()).await;
    match res {
        Ok(out) => Ok(out),
        Err(e) => {
            // K-AUTONOMY: no silent stops — durable session.error + a
            // [turn stopped] part + finalize + idle (any run failure: cap,
            // doom-window, stall, provider, cost).
            surface_run_failure(ctx, writer, session_id, &e).await;
            Err(e)
        }
    }
}

pub async fn run_prompt_with(
    ctx: &PromptContext,
    writer: &ocserve_store::Writer,
    session_id: &str,
    req: &crate::wire::PromptRequest,
    opts: RunOpts,
) -> Result<(Value, Vec<Value>)> {
    if !ocserve_store::session_exists(&ctx.db, session_id)? {
        anyhow::bail!("Session not found: {session_id}");
    }
    // K-ALWAYS: one DB read pulls this session's persisted always-grants
    // into the gate (memory-first consult afterwards).
    ctx.gate.hydrate(&ctx.db, session_id)?;
    // K-MODEL-STATE: resolved model+agent land at START (not finalize) —
    // cap/OOM/abort deaths keep the selection sticky for the next
    // payload-less send. Churn-free UPDATE (0 rows when unchanged).
    ocserve_store::persist_prompt_model(
        writer,
        session_id,
        &ctx.agent,
        &json!({"id": ctx.model_id, "providerID": ctx.provider_id, "variant": "default"})
            .to_string(),
    )?;
    // K-EFFICIENCY: RSS + reader-open probe — Drop-based so BAIL exits
    // (cap/provider errors) emit too (tail-only emission missed them:
    // proven by the cap e2e going red the first time).
    let mut probe = PhaseProbe::start();
    // ONE reader answers seq + compaction state (audit fix: pre-M6 = two
    // opens per prompt; without this the round checks added two MORE cold
    // opens on the hottest path — COMPACTION §11 hot-path rule).
    let mut pre = ocserve_store::compaction_preflight(&ctx.db, session_id)?;
    let mut seq = pre.seq;
    let mut pre_dirty = false;
    // Session-lifetime usage accumulator, seeded ONCE from the preflight row.
    // Mirrors upstream applyUsage (core/session/projector.ts:89-108): every
    // step-finish ADDS its usage to the session row; the running copy here lets
    // mid-turn/final `session.updated` events carry the truthful aggregate
    // without a mid-turn DB read. Seeded once (not re-seeded on the compaction
    // re-preflight) so a round never double-counts or loses a round's steps.
    let mut session_cost = pre.cost;
    let mut session_usage = json!({
        "input": pre.tokens_input,
        "output": pre.tokens_output,
        "reasoning": pre.tokens_reasoning,
        "cache": {"read": pre.tokens_cache_read, "write": pre.tokens_cache_write},
    });

    let model = req
        .model
        .as_ref()
        .map(|m| m.model_id.clone())
        .unwrap_or_else(|| ctx.model_id.clone());
    let agent = req
        .agent
        .clone()
        .filter(|a| !a.is_empty())
        .unwrap_or_else(|| ctx.agent.clone());

    // ---- persist user message (text parts; file parts land with M3) ----
    // Boundary decode (wire.rs) validated `messageID` against `^msg` — the
    // upstream rule (isStartsWith only; probe: bare "msg" and 5000-char ids
    // pass, no length/charset cap). Internal constructors pass None →
    // generate (upstream `input.messageID ?? MessageID.ascending()`).
    let user_msg_id = req.message_id.clone().unwrap_or_else(msg_id);
    let mut user_parts = Vec::new();
    for p in &req.parts {
        if p["type"] == "text" {
            let text = p["text"].as_str().unwrap_or("").to_string();
            user_parts.push(json!({
                "id": prt_id(),
                "sessionID": session_id,
                "messageID": user_msg_id,
                "type": "text",
                "text": text,
            }));
        }
    }
    let t_created = now_ms();
    let mut user_info = json!({
        "id": user_msg_id,
        "sessionID": session_id,
        "role": "user",
        "time": {"created": t_created},
        "summary": {"diffs": []},
        "agent": agent,
        "model": {"providerID": ctx.provider_id, "modelID": model},
    });
    // upstream UserV1 info carries the request-scoped fields when present
    // (prompt.ts:661 tools, :668 system, :669 format — undefined drops the
    // key in JSON.stringify, so absent ≠ null and we insert only when Some).
    if let Some(t) = &req.tools {
        user_info["tools"] = Value::Object(t.clone());
    }
    if let Some(s) = &req.system {
        user_info["system"] = Value::String(s.clone());
    }
    if let Some(f) = &req.format {
        user_info["format"] = f.clone();
    }
    // v1 parity: chat.message (prompt.ts:1000) fires BEFORE persistence so
    // plugins (magic-context) can mutate {message, parts} into history.
    // Fidelity note: only the REAL prompt flow fires it — upstream's
    // summarize (compaction.ts) persists its user marker through a
    // different path and never triggers chat.message. persist_user alone
    // can't distinguish (ocserve summarize persists the marker too, probe
    // 2026-10-02) — skip_history can (summarize-only): gate on BOTH.
    if opts.persist_user && !opts.skip_history {
        let out = hook_mutate(
            ctx.plugins.as_ref(),
            "chat.message",
            json!({
                "sessionID": session_id,
                "agent": agent,
                "model": model,
                "messageID": user_msg_id,
                "variant": req.variant.clone().map(Value::String).unwrap_or(Value::Null),
            }),
            json!({"message": user_info, "parts": user_parts}),
        )
        .await;
        if let Some(m) = out.get("message")
            && m.is_object()
        {
            user_info = m.clone();
        }
        if let Some(p) = out.get("parts").and_then(|p| p.as_array()) {
            user_parts = p.to_vec();
        }
    }
    if opts.persist_user {
        insert_message(
            writer,
            Some(&*ctx.blobs),
            session_id,
            &user_info,
            &user_parts,
        )
        .context("persist user message")?;
    }

    emit_session_updated(
        ctx,
        writer,
        session_id,
        json!({
            "model": {"id": model, "providerID": ctx.provider_id, "variant": "default"},
            "agent": agent,
        }),
        &mut seq,
    )?;
    if opts.persist_user {
        emit_durable(
            ctx,
            writer,
            session_id,
            "message.updated",
            json!({"sessionID": session_id, "info": user_info}),
            &mut seq,
        )?;
        for part in &user_parts {
            emit_durable(
                ctx,
                writer,
                session_id,
                "message.part.updated",
                json!({"sessionID": session_id, "part": part}),
                &mut seq,
            )?;
        }
    }
    // v1 `noReply` (prompt.ts:1069): the user message persists and the loop
    // never starts — return the user message itself (probe 2026-10-09:
    // [200] {info: role:user, parts} with a dead endpoint = no model call).
    // NAMED DIVERGENCE: upstream rewrites session permission rules from
    // `input.tools` immediately before this return (prompt.ts:1059-1067);
    // ocserve's session.permission column stores always-grant keys, not
    // PermissionV1.Rule objects — tools are decoded and persisted into
    // user_info, but the ruleset side-effect is not ported (field-probes.md).
    if req.no_reply {
        return Ok((user_info, user_parts));
    }
    emit_live(
        ctx,
        "session.status",
        json!({"sessionID": session_id, "status": {"type": "busy"}}),
    );

    // ── M6 outer compaction loop (upstream runLoop, prompt.ts:1083+) ──
    // Rounds re-filter history, run the pending engine step, then rebuild
    // messages; D1 cap counts engine rounds (COMPACTION §6).
    let mut total_cost = 0.0f64;
    let mut auto_doom = crate::compaction::AutoCompactionDoom::default();
    // fatal-death reason (D1/cost); None = generic at the final_out check
    let mut death: Option<String> = None;
    // cumulative provider↔tool rounds for THIS prompt (K-AUTONOMY: cap is
    // opt-in via env/opts — default OFF, sessions run as long as needed)
    let mut step = 0usize;
    let max_rounds = resolve_max_rounds(&opts, ctx.max_rounds);
    let cost_ceiling = resolve_cost_ceiling(&opts, ctx.cost_ceiling);
    let mut final_out: Option<(Value, Vec<Value>)> = None;
    'outer: loop {
        // refresh ONLY after the engine/trigger wrote (round 1 keeps the
        // locally-advanced seq — re-reading would be a redundant open)
        // (no clear here: Pending re-sets dirty after process; Ready falls
        // through to generation and exits — clearing was a dead assignment)
        if pre_dirty {
            pre = ocserve_store::compaction_preflight(&ctx.db, session_id)?;
            seq = pre.seq;
        }
        // loop-guard window: resets per compaction round (each round is a
        // fresh provider conversation) — DIFFERENTIATION D1
        let mut loop_win: Vec<crate::loop_guard::Entry> = Vec::new();
        match std::mem::replace(&mut pre.state, ocserve_store::PreflightState::Ready) {
            ocserve_store::PreflightState::Pending {
                anchor: aid,
                auto: pa,
                overflow: po,
            } => {
                if auto_doom.note() {
                    death = Some(
                        "automatic compaction doom-window (COMPACTION D1: more than 3 \
                         auto-compactions within 30 minutes — summarize is not working)"
                            .to_string(),
                    );
                    break 'outer;
                }
                if !crate::compaction::process(
                    ctx,
                    writer,
                    session_id,
                    &ctx.compaction,
                    pa,
                    po,
                    &aid,
                    &agent,
                )
                .await?
                {
                    break 'outer; // summarize-overflow persisted (Stopped)
                }
                pre_dirty = true;
                continue 'outer;
            }
            // manual flow's anchor+summary are the newest pair (upstream
            // loop breaks when summary.parentID == latest user,
            // prompt.ts:1101-1115) — auto never trips this: replay/
            // autocontinue append AFTER the summary.
            ocserve_store::PreflightState::SummaryExit { info, parts } => {
                final_out = Some((info, parts));
                break 'outer;
            }
            ocserve_store::PreflightState::Ready => {}
        }

        // ---- provider context ----
        let mut messages = vec![ChatMessage::text("system", ctx.system.clone())];
        if opts.skip_history {
            // transient instruction carries the serialized history (summarize)
            messages.push(ChatMessage::text(
                "user",
                opts.prelude.clone().unwrap_or_default(),
            ));
        } else {
            let history = ocserve_store::load_messages(&ctx.db, session_id, None)?;
            messages.extend(to_provider_messages(&history));
        }
        let mut tools = if opts.tools_enabled {
            ocserve_tools::schemas()
        } else {
            Vec::new()
        };
        if opts.tools_enabled
            && let Some(hub) = &ctx.mcp
        {
            // cached after first prompt (listChanged=false servers); failures
            // degrade to builtin-only tools, never block the prompt
            let mcp_tools =
                tokio::time::timeout(std::time::Duration::from_secs(30), hub.tool_schemas())
                    .await
                    .unwrap_or_else(|_| {
                        tracing::warn!("mcp tool schema fetch timed out");
                        Vec::new()
                    });
            tools.extend(mcp_tools);
        }
        // v1 visibleTools/disabled parity: deny'd tools are never sent to the
        // model (ask/allow stay visible — ask flows at exec time instead).
        tools.retain(|spec| {
            spec.pointer("/function/name")
                .and_then(|n| n.as_str())
                .map(|n| ocserve_tools::evaluate(n, "*", &ctx.rules) != "deny")
                .unwrap_or(true)
        });

        let client = Client::new(ctx.endpoint.base_url.clone(), ctx.endpoint.api_key.clone());
        // leading system messages in `messages` (starts with the single
        // pre-built system message; system.transform may reshape the prefix)
        let mut sys_count: usize = 1;
        // v1 parity: experimental.chat.messages.transform — prompt.ts:1255 (main
        // inference) AND compaction.ts:379 (summarize): ocserve funnels both
        // flows through this builder, so ONE site covers both upstream sites.
        // The system prefix is NOT exposed (upstream fires on the stored
        // conversation; system is built separately there). Live-request only —
        // DB history untouched by construction. Fail-open on shape mismatch.
        // Shape note (§17): OpenAI messages (ocserve's wire), documented
        // divergence from upstream's AI-SDK ModelMessage shape.
        if ctx.plugins.is_some() {
            let conv: Vec<ChatMessage> = messages.split_off(sys_count);
            let mt_out = hook_mutate(
                ctx.plugins.as_ref(),
                "experimental.chat.messages.transform",
                json!({}),
                json!({
                    "messages": serde_json::to_value(&conv).unwrap_or(Value::Null)
                }),
            )
            .await;
            match mt_out
                .get("messages")
                .cloned()
                .and_then(|v| serde_json::from_value::<Vec<ChatMessage>>(v).ok())
            {
                Some(parsed) => messages.extend(parsed),
                None => messages.extend(conv), // no/unparseable array → fail-open
            }
        }
        loop {
            step += 1;
            if max_rounds > 0 && step > max_rounds {
                record_rounds("capped", step);
                anyhow::bail!(
                    "turn exceeded {max_rounds} rounds (OCSERVE_PROMPT_MAX_ROUNDS — raise it or unset to disable)"
                );
            }
            // K-EFFICIENCY: one /proc read per LLM round finds WHICH phase
            // of the turn grows (turn-spike attribution).
            probe.sample_round();
            let started = Instant::now();
            // v1 parity: chat.params + chat.headers fire PER LLM request
            // (llm/request.ts:115/135) — tool-loop steps re-trigger, matching
            // upstream. Defaults = null/empty → body byte-identical when no
            // plugins are loaded (Default ChatOpts).
            // v1 parity order (llm/request.ts): system.transform (L70) runs
            // BEFORE params (L115) and headers (L135) — per LLM request, so a
            // tool-loop step re-fires with fresh system mutation. The system
            // array is the messages PREFIX: splice replaces it each iteration
            // (upstream rebuilds system per request).
            let mut sys_vec = vec![ctx.system.clone()];
            let sys_out = hook_mutate(
                ctx.plugins.as_ref(),
                "experimental.chat.system.transform",
                json!({"sessionID": session_id, "model": model}),
                json!({"system": sys_vec}),
            )
            .await;
            if let Some(arr) = sys_out.get("system").and_then(|a| a.as_array()) {
                let mut v: Vec<String> = arr
                    .iter()
                    .filter_map(|x| x.as_str().map(String::from))
                    .collect();
                if v.is_empty() {
                    v.push(ctx.system.clone());
                }
                // upstream hoist (request.ts:73-77): hook appended >1 entries
                // and kept [0] → collapse [1..] into a single entry
                if v.len() > 2 && v[0] == ctx.system {
                    let rest = v[1..].join("\n");
                    v.truncate(1);
                    v.push(rest);
                }
                sys_vec = v;
            }
            let sys_msgs: Vec<ChatMessage> = sys_vec
                .iter()
                .map(|sc| ChatMessage::text("system", sc.clone()))
                .collect();
            messages.splice(0..sys_count, sys_msgs);
            sys_count = sys_vec.len();
            let hook_in = json!({
                "sessionID": session_id,
                "agent": agent,
                "model": model,
                "provider": ctx.provider_id,
                "message": user_info,
            });
            let params_out = hook_mutate(
                ctx.plugins.as_ref(),
                "chat.params",
                hook_in.clone(),
                json!({"temperature": Value::Null, "topP": Value::Null,
                   "topK": Value::Null, "maxOutputTokens": Value::Null,
                   "options": {}}),
            )
            .await;
            let headers_out = hook_mutate(
                ctx.plugins.as_ref(),
                "chat.headers",
                hook_in,
                json!({"headers": {}}),
            )
            .await;
            let copts = ocserve_llm::ChatOpts {
                temperature: params_out.get("temperature").and_then(|v| v.as_f64()),
                top_p: params_out.get("topP").and_then(|v| v.as_f64()),
                top_k: params_out
                    .get("topK")
                    .and_then(|v| v.as_u64())
                    .map(|v| v.min(u32::MAX as u64) as u32),
                max_tokens: params_out
                    .get("maxOutputTokens")
                    .and_then(|v| v.as_u64())
                    .map(|v| v.min(u32::MAX as u64) as u32),
                options: params_out
                    .get("options")
                    .cloned()
                    .filter(Value::is_object)
                    .unwrap_or(Value::Null),
                headers: headers_out
                    .get("headers")
                    .and_then(|h| h.as_object())
                    .map(|m| {
                        m.iter()
                            .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string())))
                            .collect()
                    })
                    .unwrap_or_default(),
                tool_choice: crate::zen::tool_choice_for(client.is_zen(), opts.tools_enabled),
            };
            // zen gate: text-only rounds still send a tools array (P6 fails
            // without it), with tool_choice none (P8) — see zen.rs.
            let zen_fallback_tools =
                crate::zen::needs_fallback_tools(client.is_zen(), opts.tools_enabled)
                    .then(ocserve_tools::schemas);
            let tools_arg = if opts.tools_enabled {
                Some(&tools[..])
            } else {
                zen_fallback_tools.as_deref()
            };
            let mut llm_ttft: Option<std::time::Duration> = None;
            let mut text = String::new();
            let mut reasoning = String::new();
            let mut finish: Option<String> = None;
            let mut usage: Option<Usage> = None;
            let mut assembler = ToolCallAssembler::default();

            // ---- assistant message + streaming part identity (upstream
            // prompt.ts:1186-1201 + processor.ts:280/500): the MessageID and the
            // text/reasoning PartIDs are minted BEFORE the provider turn and the
            // assistant skeleton is published so clients (web UI, TUI, oc-remote)
            // render the message live. The SAME ids are reused at persist time.
            // The live deltas below are `message.part.delta` — the app's reducer
            // keys them by `partID` (spec `^prt`) and drops them if the part does
            // not yet exist (the "updates only after reload" bug). The skeleton is
            // emitted LAZILY on the first content event so a turn that fails with
            // no output never leaves a ghost streaming message.
            let assistant_id = msg_id();
            let turn_msg_id = user_msg_id.clone();
            let turn_started = now_ms();
            let text_part_id = prt_id();
            let mut assistant_announced = false;
            let mut live_reasoning_id: Option<String> = None;
            // announce assistant skeleton + publish a start part for `part_type`
            // exactly once per part (upstream processor.ts:280-291/500-511 order).
            macro_rules! announce_part {
                ($part_id:expr, $part_type:expr) => {{
                    if !assistant_announced {
                        emit_live(
                            ctx,
                            "message.updated",
                            json!({"sessionID": session_id, "info": assistant_message_start(
                                ctx, session_id, &turn_msg_id, &assistant_id, &agent, &model,
                                turn_started,
                            )}),
                        );
                        assistant_announced = true;
                    }
                    emit_part_start(
                        ctx, session_id, &assistant_id, $part_id, $part_type, turn_started,
                    );
                }};
            }

            // Watchdog covers BOTH hang points: response headers (.send inside
            // chat_stream) and the read loop below — a silent socket at either
            // stage must fail, never hang busy (A3).
            let mut stream = match tokio::time::timeout(
                provider_stall(),
                client.chat_stream(&model, &messages, &copts, tools_arg, Some(session_id)),
            )
            .await
            {
                Err(_) => {
                    ocserve_metrics::labeled_counter(
                        "ocserve_llm_stream_errors_total",
                        &format!("provider=\"{}\",model=\"{model}\"", ctx.provider_id),
                        1,
                    );
                    anyhow::bail!(
                        "provider stream open stalled: no response headers within {}s (watchdog)",
                        provider_stall().as_secs()
                    );
                }
                Ok(Err(e)) => {
                    ocserve_metrics::labeled_counter(
                        "ocserve_llm_stream_errors_total",
                        &format!("provider=\"{}\",model=\"{model}\"", ctx.provider_id),
                        1,
                    );
                    // M6 overflow class (upstream processor.ts:624-634): a
                    // size-rejection at request open becomes a pending anchor
                    // with overflow=true, then the outer loop re-filters.
                    let emsg = format!("{e:#}");
                    if ocserve_llm::looks_like_context_overflow(&emsg) {
                        emit_live(
                            ctx,
                            "session.error",
                            json!({"sessionID": session_id, "error": {
                                "type": "ContextOverflowError", "data": {"message": emsg}
                            }}),
                        );
                        if auto_doom.over() {
                            anyhow::bail!(
                                "context overflow inside the compaction doom-window (COMPACTION D1)"
                            );
                        }
                        // no increment: the PENDING step counts compactions (cap = N processed)
                        crate::compaction::persist_anchor(
                            ctx, writer, session_id, &agent, true, true,
                        )
                        .await?;
                        pre_dirty = true;
                        continue 'outer;
                    }
                    return Err(e).context("provider stream open");
                }
                Ok(Ok(st)) => st,
            };
            // Stall watchdog (antagonism A3): a provider that accepts then goes
            // silent would otherwise hang run_prompt forever → lock held →
            // status busy forever → later prompts queue forever. No byte for
            // 120s = error → guards unwind → session self-heals.
            loop {
                let ev = match tokio::time::timeout(provider_stall(), stream.next()).await {
                    Err(_) => anyhow::bail!(
                        "provider stream stalled: no data for {}s (watchdog)",
                        provider_stall().as_secs()
                    ),
                    Ok(None) => break,
                    Ok(Some(ev)) => ev,
                };
                match ev.context("provider stream")? {
                    StreamEvent::TextDelta(t) => {
                        if llm_ttft.is_none() {
                            llm_ttft = Some(started.elapsed());
                            // publish the text part before its first delta
                            announce_part!(&text_part_id, "text");
                        }
                        emit_live(
                            ctx,
                            "message.part.delta",
                            json!({
                                "sessionID": session_id, "messageID": assistant_id,
                                "partID": text_part_id, "field": "text", "delta": t,
                            }),
                        );
                        text.push_str(&t);
                    }
                    StreamEvent::ReasoningDelta(r) => {
                        // upstream mints one reasoning part per reasoning-start
                        // (processor.ts:280); ocserve streams a single reasoning
                        // block per turn, so one stable id published on first delta.
                        if live_reasoning_id.is_none() {
                            let rid = prt_id();
                            announce_part!(&rid, "reasoning");
                            live_reasoning_id = Some(rid);
                        }
                        emit_live(
                            ctx,
                            "message.part.delta",
                            json!({
                                "sessionID": session_id, "messageID": assistant_id,
                                "partID": live_reasoning_id.clone().unwrap_or_default(),
                                "field": "reasoning", "delta": r,
                            }),
                        );
                        reasoning.push_str(&r);
                    }
                    StreamEvent::ToolCallDelta {
                        index,
                        id,
                        name,
                        arguments_delta,
                    } => {
                        assembler.push(index, id, name, arguments_delta);
                    }
                    StreamEvent::Done {
                        finish: f,
                        usage: u,
                    } => {
                        if f.is_some() {
                            finish = f;
                        }
                        if u.is_some() {
                            usage = u;
                        }
                    }
                }
            }
            let tool_calls = assembler.finish();
            let elapsed_ms = started.elapsed().as_millis() as i64;
            {
                let llm_label = format!("provider=\"{}\",model=\"{model}\"", ctx.provider_id);
                ocserve_metrics::observe(
                    "ocserve_llm_request_duration_seconds",
                    &llm_label,
                    started.elapsed().as_micros() as u64,
                );
                if let Some(ttft) = llm_ttft {
                    ocserve_metrics::observe(
                        "ocserve_llm_ttft_seconds",
                        &llm_label,
                        ttft.as_micros() as u64,
                    );
                }
            }
            let t_done = now_ms();
            let finish_reason = finish.unwrap_or_else(|| "stop".into());
            let u = usage.clone().unwrap_or_default();
            if u.cached_tokens > 0 {
                ocserve_metrics::labeled_counter(
                    "ocserve_llm_cache_tokens_total",
                    &format!("provider=\"{}\",dir=\"read\"", ctx.provider_id),
                    u.cached_tokens,
                );
            }
            let step_cost = compute_cost(&ctx.endpoint.pricing, &u);
            total_cost += step_cost;
            // Session-lifetime accumulation (upstream applyUsage): every
            // step-finish part adds its usage to the session row. Keep the
            // in-memory copy in step with the queued DB op (writer.write is
            // ack-synchronous, so op + copy are always consistent).
            let step_tokens = message_tokens(&u);
            ocserve_store::accumulate_session_usage(writer, session_id, &step_tokens, step_cost)?;
            {
                let bump = |agg: &mut Value, key: &str, v: i64| {
                    agg[key] = (agg[key].as_i64().unwrap_or(0) + v).into();
                };
                bump(
                    &mut session_usage,
                    "input",
                    step_tokens["input"].as_i64().unwrap_or(0),
                );
                bump(
                    &mut session_usage,
                    "output",
                    step_tokens["output"].as_i64().unwrap_or(0),
                );
                bump(
                    &mut session_usage,
                    "reasoning",
                    step_tokens["reasoning"].as_i64().unwrap_or(0),
                );
                let read =
                    session_usage["cache"]["read"].as_i64().unwrap_or(0)
                        + step_tokens["cache"]["read"].as_i64().unwrap_or(0);
                let write =
                    session_usage["cache"]["write"].as_i64().unwrap_or(0)
                        + step_tokens["cache"]["write"].as_i64().unwrap_or(0);
                session_usage["cache"]["read"] = read.into();
                session_usage["cache"]["write"] = write.into();
                session_cost += step_cost;
            }
            if cost_ceiling > 0.0 && total_cost >= cost_ceiling {
                death = Some(format!(
                    "prompt reached the cost ceiling ${cost_ceiling:.4} (OCSERVE_PROMPT_MAX_COST_USD)"
                ));
                break 'outer;
            }
            // assistant_id / text_part_id / live_reasoning_id are minted once
            // before the provider turn (see the pre-alloc block above) and reused
            // here so the streamed part and the persisted part share ONE id.

            // ---- tool-call turn ----
            if finish_reason == "tool_calls" && !tool_calls.is_empty() {
                let mut parts = vec![json!({
                    "type": "step-start", "id": prt_id(),
                    "sessionID": session_id, "messageID": assistant_id,
                })];
                if !reasoning.is_empty() {
                    parts.push(json!({
                        "type": "reasoning", "text": reasoning,
                        "time": {"start": t_done - elapsed_ms, "end": t_done},
                        "id": live_reasoning_id.clone().unwrap_or_else(prt_id),
                        "sessionID": session_id, "messageID": assistant_id,
                    }));
                }
                if !text.is_empty() {
                    text = hook_mutate(
                        ctx.plugins.as_ref(),
                        "experimental.text.complete",
                        json!({
                            "sessionID": session_id,
                            "messageID": assistant_id,
                            "partID": text_part_id,
                        }),
                        json!({"text": text}),
                    )
                    .await
                    .get("text")
                    .and_then(|t| t.as_str())
                    .map(String::from)
                    .unwrap_or(text);
                    parts.push(json!({
                        "type": "text", "text": text,
                        "time": {"start": t_done - elapsed_ms, "end": t_done},
                        "id": text_part_id, "sessionID": session_id, "messageID": assistant_id,
                    }));
                }

                let mut provider_tool_results = Vec::new();
                for call in &tool_calls {
                    let part_id = prt_id();
                    let mut running = json!({
                        "type": "tool", "id": part_id,
                        "sessionID": session_id, "messageID": assistant_id,
                        "callID": call.id, "tool": call.name,
                        "state": {
                            "status": "running",
                            "input": serde_json::from_str::<Value>(&call.arguments)
                                .unwrap_or_else(|_| json!({"raw": call.arguments})),
                            "time": {"start": now_ms()},
                        },
                    });
                    emit_durable(
                        ctx,
                        writer,
                        session_id,
                        "message.part.updated",
                        json!({"sessionID": session_id, "part": running}),
                        &mut seq,
                    )?;

                    // ---- question tool: own rendezvous, no permission gate
                    // (upstream QuestionTool never ctx.ask()s) ----
                    if call.name == "question" {
                        let (output, state) = question_tool_state(
                            ctx,
                            writer,
                            session_id,
                            &assistant_id,
                            &call.id,
                            &call.arguments,
                            &mut seq,
                        )
                        .await?;
                        running["state"] = state;
                        if let Some(plug) = &ctx.plugins {
                            let hook_in = json!({
                                "sessionID": session_id,
                                "messageID": assistant_id,
                                "callID": call.id,
                                "tool": call.name,
                                "agent": agent,
                                "modelID": model,
                                "args": serde_json::from_str::<Value>(&call.arguments)
                                    .unwrap_or_else(|_| json!({})),
                                "session": {"id": session_id},
                            });
                            let mut guard = plug.lock().await;
                            let hook_t0 = std::time::Instant::now();
                            let hook_res = guard
                                .trigger("tool.execute.after", hook_in, json!({"output": output}))
                                .await;
                            ocserve_metrics::observe(
                                "ocserve_plugin_hook_duration_seconds",
                                "hook=\"tool.execute.after\",result=\"ok\"",
                                hook_t0.elapsed().as_micros() as u64,
                            );
                            if let Err(e) = &hook_res {
                                ocserve_metrics::labeled_counter(
                                    "ocserve_plugin_hook_errors_total",
                                    "hook=\"tool.execute.after\"",
                                    1,
                                );
                                tracing::warn!("plugin tool.execute.after: {e:#}");
                            }
                        }
                        emit_durable(
                            ctx,
                            writer,
                            session_id,
                            "message.part.updated",
                            json!({"sessionID": session_id, "part": running}),
                            &mut seq,
                        )?;
                        parts.push(running);
                        provider_tool_results.push((call.id.clone(), output));
                        continue;
                    }

                    // ---- permission gate (ordered ask sequence) ----
                    // Upstream tools call ctx.ask() zero-or-more times before
                    // executing; each ask is evaluated against the agent rules
                    // plus this session's "always" grants. deny → blocked;
                    // ask → emit + await reply (once/always/reject); always →
                    // grant the ask's `always` patterns as wildcard rules.
                    let builtin = ocserve_tools::schemas()
                        .iter()
                        .any(|s| s["function"]["name"] == call.name);
                    let input_value: serde_json::Value =
                        serde_json::from_str(&call.arguments).unwrap_or_else(|_| json!({}));
                    let asks = ocserve_tools::permission_asks::asks_for(
                        &call.name,
                        &input_value,
                        std::path::Path::new(&ctx.directory),
                    );
                    let mut allowed = true;
                    let mut denied = false;
                    for ask in &asks {
                        // granted-always short-circuits the ask
                        let effect = if ask
                            .patterns
                            .iter()
                            .all(|p| ctx.gate.check_always(session_id, &ask.permission, p))
                        {
                            "allow"
                        } else {
                            let mut eff = "allow";
                            for p in &ask.patterns {
                                match ocserve_tools::evaluate(&ask.permission, p, &ctx.rules)
                                    .as_str()
                                {
                                    "deny" => {
                                        eff = "deny";
                                        break;
                                    }
                                    "ask" if eff != "deny" => eff = "ask",
                                    _ => {}
                                }
                            }
                            eff
                        };
                        if effect == "deny" {
                            denied = true;
                            allowed = false;
                            break;
                        }
                        if effect == "ask" {
                            let perm_id = crate::ids::per_id();
                            let request = json!({
                                "id": perm_id,
                                "sessionID": session_id,
                                "permission": ask.permission,
                                "patterns": ask.patterns,
                                "always": ask.always,
                                "metadata": ask.metadata,
                                "tool": {"messageID": assistant_id, "callID": call.id},
                            });
                            let (rx, _perm_guard) =
                                ctx.gate.clone().register(&perm_id, request.clone());
                            emit_durable(
                                ctx,
                                writer,
                                session_id,
                                "permission.asked",
                                request.clone(),
                                &mut seq,
                            )?;
                            let reply = ctx.gate.wait(&perm_id, rx).await;
                            emit_durable(
                                ctx,
                                writer,
                                session_id,
                                "permission.replied",
                                json!({"sessionID": session_id, "requestID": perm_id, "reply": reply}),
                                &mut seq,
                            )?;
                            if reply == "reject" {
                                allowed = false;
                                break;
                            }
                            if reply == "always" {
                                for pat in &ask.always {
                                    ctx.gate.grant_always(session_id, &ask.permission, pat);
                                    // K-ALWAYS: persist past restart
                                    if let Err(e) = ocserve_store::session_grant_always(
                                        writer,
                                        &ctx.db,
                                        session_id,
                                        &ask.permission,
                                        pat,
                                    ) {
                                        tracing::error!(
                                            "persist always grant {}:{pat}: {e:#}",
                                            ask.permission
                                        );
                                    }
                                }
                            }
                        }
                    }
                    if denied {
                        // upstream: a hard deny throws → the turn is blocked
                        // (processor.ts ctx.blocked = shouldBreak).
                        if crate::loop_guard::deny_blocks_turn() {
                            emit_durable(
                                ctx,
                                writer,
                                session_id,
                                "message.part.updated",
                                json!({"sessionID": session_id, "part": {
                                    "type": "tool", "id": prt_id(),
                                    "sessionID": session_id, "messageID": assistant_id,
                                    "callID": call.id, "tool": call.name,
                                    "state": {"status": "error", "input": input_value,
                                        "error": "Permission denied", "time": {"start": now_ms(), "end": now_ms()}},
                                }}),
                                &mut seq,
                            )?;
                            anyhow::bail!("tool {} denied by permission rules", call.name);
                        }
                    }

                    // loop guard, pre-exec (D1/P1b): upstream parity asks
                    // permission "doom_loop" on 3× identical tool+input
                    // (processor.ts:29,356-383); oscillation rides the same
                    // flow with additive metadata.class only.
                    if allowed && crate::loop_guard::asks_enabled() {
                        let input_key = crate::loop_guard::input_key(&call.arguments);
                        // K4: self-throttling steps (sleep/tail -f) can't be a
                        // hot loop — never trip repeat on them overnight.
                        if !crate::loop_guard::self_throttled(&call.arguments)
                            && let Some(class) =
                                crate::loop_guard::check_pre(&loop_win, &call.name, &input_key)
                        {
                            ocserve_metrics::labeled_counter(
                                "ocserve_agent_health_total",
                                &format!("class=\"{}\"", class.as_str()),
                                1,
                            );
                            let ok = loop_guard_ask(
                                ctx,
                                writer,
                                session_id,
                                class,
                                &call.name,
                                &input_key,
                                &assistant_id,
                                &call.id,
                                &mut seq,
                            )
                            .await?;
                            ocserve_metrics::labeled_counter(
                                "ocserve_agent_health_action_total",
                                if ok {
                                    "action=\"approved\""
                                } else {
                                    "action=\"denied\""
                                },
                                1,
                            );
                            if !ok {
                                allowed = false;
                            }
                        }
                    }

                    // ---- execute ----
                    let exec_start = now_ms();
                    let (mut output, meta, title, is_err) = if allowed {
                        // v1 parity: tool.execute.before (tools.ts:107) — input
                        // {tool, sessionID, callID} (upstream-exact, minimal),
                        // output {args}; MUTATED args are what executes. The
                        // part state keeps the original model args (upstream
                        // builds part state pre-hook too).
                        let mut input = serde_json::from_str::<Value>(&call.arguments)
                            .unwrap_or_else(|_| json!({}));
                        let args_out = hook_mutate(
                            ctx.plugins.as_ref(),
                            "tool.execute.before",
                            json!({
                                "tool": call.name,
                                "sessionID": session_id,
                                "callID": call.id,
                            }),
                            json!({"args": input}),
                        )
                        .await;
                        if let Some(a) = args_out.get("args")
                            && a.is_object()
                        {
                            input = a.clone();
                        }
                        // G1 soundness: `tool.execute.before` runs AFTER the
                        // permission gate, so a plugin (compromised via poisoned
                        // tool output) could widen the approved args (e.g. `rm x`
                        // → `rm -rf /`) with no re-evaluation. Re-derive the asks
                        // for the MUTATED args; any ask not already covered by an
                        // always-grant or an `allow` rule reverts the mutation
                        // (defense-in-depth; upstream does not re-check).
                        if input != input_value {
                            let mutated_asks = ocserve_tools::permission_asks::asks_for(
                                &call.name,
                                &input,
                                std::path::Path::new(&ctx.directory),
                            );
                            let widened = mutated_asks.iter().any(|ask| {
                                let covered_always = ask
                                    .patterns
                                    .iter()
                                    .all(|p| ctx.gate.check_always(session_id, &ask.permission, p));
                                if covered_always {
                                    return false;
                                }
                                ask.patterns.iter().any(|p| {
                                    ocserve_tools::evaluate(&ask.permission, p, &ctx.rules)
                                        != "allow"
                                })
                            });
                            if widened {
                                ocserve_metrics::labeled_counter(
                                    "ocserve_agent_health_total",
                                    "class=\"plugin_args_widened\"",
                                    1,
                                );
                                tracing::warn!(
                                    "tool.execute.before widened {call} args past the \
                                     approved set — reverting the mutation",
                                    call = call.name
                                );
                                input = input_value.clone();
                            }
                        }
                        // K-EFFICIENCY: tool execution runs OFF the async
                        // workers (spawn_blocking) — a blocking `sleep 570`
                        // or a stalled child previously pinned one of the 8
                        // tokio workers and starved EVERY route (the observed
                        // multi-minute "server hung" windows).
                        let exec = if builtin {
                            spawn_tool(&call.name, input, &ctx.directory).await
                        } else if let Some(hub) = &ctx.mcp {
                            match hub.call(&call.name, input.clone()).await {
                                Some(Ok(text)) => Ok(ocserve_tools::ToolResult {
                                    output: text,
                                    truncated: false,
                                    exit: None,
                                    title: call.name.clone(),
                                    error: false,
                                    metadata: None,
                                }),
                                Some(Err(e)) => Err(e),
                                None => spawn_tool(&call.name, input, &ctx.directory).await,
                            }
                        } else {
                            spawn_tool(&call.name, input, &ctx.directory).await
                        };
                        match exec {
                            Ok(r) => {
                                // todowrite supplies {todos} metadata; everything
                                // else keeps the bash-style template
                                let meta = r.metadata.unwrap_or_else(|| {
                                    json!({
                                        "output": "",
                                        "exit": r.exit,
                                        "truncated": r.truncated,
                                    })
                                });
                                (r.output, meta, r.title, r.error)
                            }
                            Err(e) => (format!("Error: {e:#}"), json!({}), call.name.clone(), true),
                        }
                    } else {
                        (
                            "Permission denied".to_string(),
                            json!({}),
                            call.name.clone(),
                            true,
                        )
                    };
                    // P2/P2b: runtime trust scan of MCP (non-builtin) responses —
                    // the channel connect-time review never sees (OWASP MCP03).
                    // MUST run BEFORE state assignment: history rebuild reads
                    // state.output, so enforce has to withhold there too or the
                    // raw response leaks back into the prompt next turn.
                    // Observe (default): metric + log, output unchanged.
                    if !builtin {
                        output = ocserve_mcp::McpHub::guard_output(&call.name, output);
                    }
                    running["state"] = json!({
                        "status": if is_err { "error" } else { "completed" },
                        "input": serde_json::from_str::<Value>(&call.arguments)
                            .unwrap_or_else(|_| json!({"raw": call.arguments})),
                        "output": output,
                        "title": title,
                        // meta = bash template {output:"",exit,truncated} OR the
                        // tool's custom metadata (todowrite {todos}) — the old
                        // destructure here DROPPED custom keys (field bug: todos
                        // never reached state/persist — caught by live verify)
                        "metadata": meta,
                        "time": {"start": exec_start, "end": now_ms()},
                    });
                    // metadata.output mirrors output (capture fact) — except
                    // todowrite parts, whose metadata is {todos} (v1 Output)
                    if running["state"]["metadata"].get("todos").is_none() {
                        running["state"]["metadata"]["output"] = running["state"]["output"].clone();
                    }
                    // todowrite: REPLACE the session todo list (v1
                    // SessionTodo.update) + emit todo.updated (durable+s twin) —
                    // oc-remote's todos panel reads GET /todo and this event
                    if call.name == "todowrite"
                        && running["state"]["status"] == "completed"
                        && let Some(todos) = running["state"]["metadata"]["todos"].as_array()
                    {
                        let todos = todos.clone();
                        let now = now_ms();
                        let mut ops = vec![ocserve_store::WriteOp::Sql {
                            sql: "DELETE FROM todo WHERE session_id = ?1".into(),
                            params: vec![session_id.into()],
                        }];
                        for (i, t) in todos.iter().enumerate() {
                            ops.push(ocserve_store::WriteOp::Sql {
                                sql: "INSERT INTO todo (id, session_id, content, status, priority, time_created, time_updated)                                       VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?6)".into(),
                                params: vec![
                                    format!("{session_id}_t{i}").into(),
                                    session_id.into(),
                                    t["content"].as_str().unwrap_or_default().into(),
                                    t["status"].as_str().unwrap_or("pending").into(),
                                    t["priority"].as_str().unwrap_or("").into(),
                                    now.into(),
                                ],
                            });
                        }
                        writer.write(ops)?;
                        emit_durable(
                            ctx,
                            writer,
                            session_id,
                            "todo.updated",
                            json!({"sessionID": session_id, "todos": todos}),
                            &mut seq,
                        )?;
                    }

                    // v1 hook: tool.execute.after (sequential in-host; failures are
                    // logged host-side, never block the tool result — M4b)
                    if let Some(plug) = &ctx.plugins {
                        let hook_in = json!({
                            "sessionID": session_id,
                            "messageID": assistant_id,
                            "callID": call.id,
                            "tool": call.name,
                            "agent": agent,
                            "modelID": model,
                            "args": serde_json::from_str::<Value>(&call.arguments)
                                .unwrap_or_else(|_| json!({})),
                            "session": {"id": session_id},
                        });
                        let mut guard = plug.lock().await;
                        let hook_t0 = std::time::Instant::now();
                        let hook_res = guard
                            .trigger("tool.execute.after", hook_in, json!({"output": output}))
                            .await;
                        ocserve_metrics::observe(
                            "ocserve_plugin_hook_duration_seconds",
                            "hook=\"tool.execute.after\",result=\"ok\"",
                            hook_t0.elapsed().as_micros() as u64,
                        );
                        if let Err(e) = &hook_res {
                            ocserve_metrics::labeled_counter(
                                "ocserve_plugin_hook_errors_total",
                                "hook=\"tool.execute.after\"",
                                1,
                            );
                            tracing::warn!("plugin tool.execute.after: {e:#}");
                        }
                    }
                    emit_durable(
                        ctx,
                        writer,
                        session_id,
                        "message.part.updated",
                        json!({"sessionID": session_id, "part": running}),
                        &mut seq,
                    )?;
                    {
                        let out_str = running["state"]["output"].as_str().unwrap_or_default();
                        // K4: throttled steps stay OUT of the window (their
                        // repetition is rate-limited by their own sleep).
                        if !crate::loop_guard::self_throttled(&call.arguments) {
                            loop_win.push(crate::loop_guard::entry(
                                &call.name,
                                &call.arguments,
                                out_str,
                                is_err,
                            ));
                        }
                    }
                    parts.push(running);
                    // live-site sift: same fn + same raw bytes as the history
                    // rebuild => deterministic identical send both times (P1c)
                    let bound = crate::sift_boundary::maybe_sift(&call.name, &output);
                    provider_tool_results.push((call.id.clone(), bound.to_string()));
                    // loop guard, post-exec (D1/P1b): spiral asks (declined →
                    // prompt aborts, session self-heals like the stall path);
                    // error-storm is metric-only — iterating on a failing test
                    // is legitimate work and asking would be fatigue.
                    if let Some(class) = crate::loop_guard::check_post(&loop_win) {
                        ocserve_metrics::labeled_counter(
                            "ocserve_agent_health_total",
                            &format!("class=\"{}\"", class.as_str()),
                            1,
                        );
                        if class != crate::loop_guard::Class::ErrorStorm
                            && crate::loop_guard::asks_enabled()
                        {
                            let input_key = crate::loop_guard::input_key(&call.arguments);
                            let ok = loop_guard_ask(
                                ctx,
                                writer,
                                session_id,
                                class,
                                &call.name,
                                &input_key,
                                &assistant_id,
                                &call.id,
                                &mut seq,
                            )
                            .await?;
                            ocserve_metrics::labeled_counter(
                                "ocserve_agent_health_action_total",
                                if ok {
                                    "action=\"approved\""
                                } else {
                                    "action=\"denied\""
                                },
                                1,
                            );
                            if !ok {
                                anyhow::bail!(
                                    "loop guard: {} detected and declined by user (set OCSERVE_LOOP_GUARD=0 to disable asks)",
                                    class.as_str()
                                );
                            }
                        }
                    }
                }

                // persist the tool-step assistant message (parent = user message)
                let step_info = json!({
                    "parentID": user_msg_id,
                    "role": "assistant",
                    "mode": "primary",
                    "agent": agent,
                    "path": {"cwd": ctx.directory, "root": "/"},
                    "cost": step_cost,
                    "tokens": step_tokens,
                    "modelID": model,
                    "providerID": ctx.provider_id,
                    "time": {"created": t_done - elapsed_ms, "completed": t_done},
                    "finish": "tool-calls",
                    "id": assistant_id,
                    "sessionID": session_id,
                });
                parts.push(json!({
                    "reason": "tool-calls", "type": "step-finish",
                    "tokens": step_info["tokens"], "cost": step_cost,
                    "id": prt_id(), "sessionID": session_id, "messageID": assistant_id,
                }));
                insert_message(writer, Some(&*ctx.blobs), session_id, &step_info, &parts)
                    .context("persist tool-step message")?;
                for part in &parts {
                    emit_durable(
                        ctx,
                        writer,
                        session_id,
                        "message.part.updated",
                        json!({"sessionID": session_id, "part": part}),
                        &mut seq,
                    )?;
                }
                emit_durable(
                    ctx,
                    writer,
                    session_id,
                    "message.updated",
                    json!({"sessionID": session_id, "info": step_info}),
                    &mut seq,
                )?;
                emit_session_updated(
                    ctx,
                    writer,
                    session_id,
                    json!({
                        "cost": session_cost,
                        "tokens": session_usage.clone(),
                        "time": {"updated": t_done},
                    }),
                    &mut seq,
                )?;

                // extend provider conversation for the next turn
                let calls_json: Vec<Value> = tool_calls
                    .iter()
                    .map(|c| {
                        json!({
                            "id": c.id,
                            "type": "function",
                            "function": {"name": c.name, "arguments": c.arguments},
                        })
                    })
                    .collect();
                messages.push(ChatMessage::assistant_with_tools(text, calls_json));
                for (cid, out) in provider_tool_results {
                    messages.push(ChatMessage::tool_result(cid, out));
                }
                continue;
            }

            // ---- final turn ----
            let mut parts = vec![json!({
                "type": "step-start", "id": prt_id(),
                "sessionID": session_id, "messageID": assistant_id,
            })];
            if !reasoning.is_empty() {
                parts.push(json!({
                    "type": "reasoning", "text": reasoning,
                    "time": {"start": t_done - elapsed_ms, "end": t_done},
                    // reuse the reasoning id the live deltas streamed to (if any)
                    "id": live_reasoning_id.clone().unwrap_or_else(prt_id),
                    "sessionID": session_id, "messageID": assistant_id,
                }));
            }
            if !text.is_empty() {
                // reuse the id the live text deltas streamed to so the client
                // reconciles the streamed part with the persisted one (upstream
                // updates the SAME currentText part at text-end, processor.ts:544)
                let text_part_id = text_part_id.clone();
                text = hook_mutate(
                    ctx.plugins.as_ref(),
                    "experimental.text.complete",
                    json!({
                        "sessionID": session_id,
                        "messageID": assistant_id,
                        "partID": text_part_id,
                    }),
                    json!({"text": text}),
                )
                .await
                .get("text")
                .and_then(|t| t.as_str())
                .map(String::from)
                .unwrap_or(text);
                parts.push(json!({
                "type": "text", "text": text, "time": {"start": t_done - elapsed_ms, "end": t_done},
                "id": text_part_id, "sessionID": session_id, "messageID": assistant_id,
            }));
            } else {
                parts.push(json!({
                "type": "text", "text": text, "time": {"start": t_done - elapsed_ms, "end": t_done},
                "id": prt_id(), "sessionID": session_id, "messageID": assistant_id,
            }));
            }
            let assistant_info = json!({
                "parentID": user_msg_id,
                "role": "assistant",
                "mode": "primary",
                "agent": agent,
                "path": {"cwd": ctx.directory, "root": "/"},
                // Per-step cost (probed against freeze: every assistant message
                // carries THAT step's cost, not the turn sum — the web UI's
                // context meter and any other per-message reader keys off it).
                "cost": step_cost,
                // Per-step tokens (upstream processor.ts:459 REPLACES the
                // message tokens each step-finish). The old whole-turn sum here
                // is what produced the >100% context meter: the sum of N steps'
                // prompts (input+cache read each call) far exceeds one call's
                // context window.
                "tokens": step_tokens,
                "modelID": model,
                "providerID": ctx.provider_id,
                "time": {"created": t_done - elapsed_ms, "completed": t_done},
                "finish": finish_reason,
                "id": assistant_id,
                "sessionID": session_id,
            });
            parts.push(json!({
                "reason": finish_reason, "type": "step-finish",
                "tokens": assistant_info["tokens"], "cost": step_cost,
                "id": prt_id(), "sessionID": session_id, "messageID": assistant_id,
            }));
            insert_message(
                writer,
                Some(&*ctx.blobs),
                session_id,
                &assistant_info,
                &parts,
            )
            .context("persist assistant message")?;

            for part in &parts {
                emit_durable(
                    ctx,
                    writer,
                    session_id,
                    "message.part.updated",
                    json!({"sessionID": session_id, "part": part}),
                    &mut seq,
                )?;
            }
            emit_durable(
                ctx,
                writer,
                session_id,
                "message.updated",
                json!({"sessionID": session_id, "info": assistant_info}),
                &mut seq,
            )?;
            emit_session_updated(
                ctx,
                writer,
                session_id,
                json!({
                    "cost": session_cost,
                    // Session-lifetime aggregate (upstream applyUsage), NOT the
                    // assistant message's step tokens. Session.tokens has no
                    // `total` (additionalProperties:false) — `session_usage` is
                    // already in that shape.
                    "tokens": session_usage.clone(),
                    "time": {"updated": t_done},
                }),
                &mut seq,
            )?;
            // ── M6 post-turn trigger (upstream prompt.ts:1160-1167 + overflow.ts
            // isOverflow): the count is the STEP's tokens — `total` when the
            // provider reports it, else the component sum (upstream's sum omits
            // reasoning). The old whole-turn sum fired compaction ~Nx early.
            let a_total = if u.total_tokens > 0 {
                u.total_tokens as i64
            } else {
                step_tokens["input"].as_i64().unwrap_or(0)
                    + step_tokens["output"].as_i64().unwrap_or(0)
                    + step_tokens["cache"]["read"].as_i64().unwrap_or(0)
                    + step_tokens["cache"]["write"].as_i64().unwrap_or(0)
            };
            let usable_t = crate::compact::usable(
                &ctx.compaction,
                ctx.model_limit["input"].as_i64().unwrap_or(0),
                ctx.model_limit["context"].as_i64().unwrap_or(0),
                ctx.model_limit["output"].as_i64().unwrap_or(0),
            );
            if ctx.compaction.auto && crate::compact::is_overflow(a_total, usable_t) {
                if auto_doom.over() {
                    emit_live(
                        ctx,
                        "session.error",
                        json!({"sessionID": session_id, "error": {
                            "type": "ContextOverflowError",
                            "data": {"message": "automatic compaction doom-window reached (COMPACTION D1) — answer kept"}
                        }}),
                    );
                } else {
                    crate::compaction::persist_anchor(ctx, writer, session_id, &agent, true, false)
                        .await?;
                    pre_dirty = true;
                    continue 'outer;
                }
            }
            final_out = Some((assistant_info, parts));
            break 'outer;
        } // inner step loop
    } // 'outer compaction loop

    // K-AUTONOMY telemetry: uncensored rounds/turn distribution (the old
    // flat cap made this unmeasurable — right-censored at 25).
    record_rounds(if final_out.is_some() { "done" } else { "error" }, step);

    // ---- finalize ONCE after any outer exit (idle trio + session row) ----
    let t_end = now_ms();
    emit_live(
        ctx,
        "session.status",
        json!({"sessionID": session_id, "status": {"type": "idle"}}),
    );
    emit_live(
        ctx,
        "session.diff",
        json!({"sessionID": session_id, "diff": []}),
    );
    emit_live(ctx, "session.idle", json!({"sessionID": session_id}));
    ocserve_store::finalize_session_prompt(
        writer,
        session_id,
        &ocserve_store::PromptStats {
            agent: &agent,
            model_json: &json!({
                "id": model,
                "providerID": ctx.provider_id,
                "variant": "default"
            })
            .to_string(),
            time_updated: t_end,
        },
    )
    .context("finalize session prompt row")?;
    // (rss/opens gauges emit from PhaseProbe::Drop — every exit path.)
    // K-TITLE: auto-title only reached on success, and only for sessions
    // STILL carrying a default title (preflight rides title — named
    // sessions skip the first_user_text read entirely).
    if opts.auto_title && final_out.is_some() && is_default_title(&pre.title) {
        auto_retag(ctx, writer, session_id);
    }
    let death_msg = death.unwrap_or_else(|| "prompt produced no assistant message".to_string());
    final_out.ok_or_else(|| anyhow::anyhow!("{death_msg}"))
}

// ---- K-AUTONOMY / K-TITLE helpers (pure where possible — unit-tested) ----

/// RunOpts (per-call) beats the ambient AppState knob (env-at-boot;
/// OCSERVE_PROMPT_MAX_ROUNDS, 0 = unlimited). Pure — unit-tested without
/// touching the process env (edition 2024 set_var would race parallel tests).
pub(crate) fn resolve_max_rounds(opts: &RunOpts, ambient: usize) -> usize {
    opts.max_rounds.unwrap_or(ambient)
}

pub(crate) fn resolve_cost_ceiling(opts: &RunOpts, ambient: f64) -> f64 {
    opts.cost_ceiling.unwrap_or(ambient)
}

/// K-AUTONOMY telemetry: pre-bucketed labeled counters (the metrics crate
/// has duration histograms only — rounds are counts, so buckets are labels).
fn record_rounds(finish: &str, rounds: usize) {
    let bucket = match rounds {
        0..=9 => "0-9",
        10..=19 => "10-19",
        20..=39 => "20-39",
        40..=79 => "40-79",
        80..=159 => "80-159",
        _ => "160+",
    };
    ocserve_metrics::labeled_counter(
        "ocserve_prompt_rounds_total",
        &format!("bucket=\"{bucket}\",finish=\"{finish}\""),
        1,
    );
}

/// D-TITLE-1 (ocserve-only): derive a session title from its first user
/// message — collapse whitespace, cap at 48 BYTES on a char boundary,
/// trim a mid-word cut, ellipsis when truncated. None = nothing usable.
pub(crate) fn auto_title_from(text: &str) -> Option<String> {
    let flat = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.is_empty() {
        return None;
    }
    let mut out = String::new();
    let mut cut = false;
    for (i, ch) in flat.char_indices() {
        if i >= 48 {
            cut = true;
            break;
        }
        out.push(ch);
    }
    if cut {
        if let Some(sp) = out.rfind(' ')
            && sp > 24
        {
            out.truncate(sp);
        }
        out.push('…');
    }
    Some(out)
}

/// Freeze default-title shapes (session.ts parentTitlePrefix + ISO, or the
/// pre-fix empty title) — everything else is user-named (never retagged).
pub(crate) fn is_default_title(title: &str) -> bool {
    title.is_empty() || title.starts_with("New session - ")
}

/// K-TITLE: name a still-default session from its first user message.
/// Retag is conditional in SQL (empty or `New session - %` only) and the
/// event mirrors prompt's partial session.updated shape ({id, title}).
fn auto_retag(ctx: &PromptContext, writer: &ocserve_store::Writer, sid: &str) {
    let text = match ocserve_store::first_user_text(&ctx.db, sid, Some(&*ctx.blobs)) {
        Ok(Some(t)) => t,
        Ok(None) => return,
        Err(e) => {
            tracing::warn!("auto-title read failed: {e:#}");
            return;
        }
    };
    let Some(title) = auto_title_from(&text) else {
        return;
    };
    match ocserve_store::retag_default_title(writer, sid, &title) {
        Ok(true) => {
            // full session (not the old partial {id,title}) — clients merge
            // the event into their store; a partial would drop required keys.
            let info = ocserve_store::session_wire_with(&ctx.db, sid, json!({"title": title}))
                .ok()
                .flatten()
                .unwrap_or_else(|| json!({"id": sid, "title": title}));
            ctx.bus.publish(frame(
                &ctx.directory,
                "session.updated",
                json!({"sessionID": sid, "info": info}),
            ));
        }
        Ok(false) => {} // named by the user already — never overwrite
        Err(e) => tracing::warn!("auto-title retag failed: {e:#}"),
    }
}

/// K-AUTONOMY surfacing for EVERY async/sync run failure (the "just
/// stopped, no error" class): durable `[turn stopped]` part on the last
/// assistant message (+ finalize when it was incomplete), durable
/// session.error (oc-remote toast — shape matches the spec
/// EventSessionError: `{name, data:{message}}`), live idle trio
/// so status consumers settle (may repeat the post-loop emits on
/// break-path deaths — idempotent events).
async fn surface_run_failure(
    ctx: &PromptContext,
    writer: &ocserve_store::Writer,
    sid: &str,
    err: &anyhow::Error,
) {
    let full = format!("{err:#}");
    let short: String = full.chars().take(2000).collect();
    let pid = crate::ids::prt_id();
    match ocserve_store::mark_turn_stopped(writer, &ctx.db, sid, &pid, &short) {
        Ok(Some(part)) => {
            ctx.bus.publish(frame(
                &ctx.directory,
                "message.part.updated",
                json!({"sessionID": sid, "part": part}),
            ));
        }
        Ok(None) => {} // died before any assistant message — event below is the trace
        Err(e) => tracing::error!("surface_run_failure part: {e:#}"),
    }
    let mut eseq = ocserve_store::next_event_seq(&ctx.db, sid).unwrap_or(1);
    if let Err(e) = emit_durable(
        ctx,
        writer,
        sid,
        "session.error",
        json!({
            "sessionID": sid,
            "error": {"name": "UnknownError", "data": {"message": short}}
        }),
        &mut eseq,
    ) {
        tracing::error!("surface_run_failure session.error: {e:#}");
    }
    emit_live(
        ctx,
        "session.status",
        json!({"sessionID": sid, "status": {"type": "idle"}}),
    );
    emit_live(ctx, "session.diff", json!({"sessionID": sid, "diff": []}));
    emit_live(ctx, "session.idle", json!({"sessionID": sid}));
}

/// The `question` tool: register → `question.asked` → await bounded reply →
/// `question.replied|rejected` → tool state + model-facing output (v1
/// QuestionTool verbatim formatting; bypasses the permission gate — upstream
/// QuestionTool never ctx.ask()s).
async fn question_tool_state(
    ctx: &PromptContext,
    writer: &ocserve_store::Writer,
    session_id: &str,
    assistant_id: &str,
    call_id: &str,
    arguments: &str,
    seq: &mut i64,
) -> Result<(String, Value)> {
    let input: Value = serde_json::from_str(arguments).unwrap_or_else(|_| json!({}));
    let questions = input.get("questions").cloned().unwrap_or(Value::Null);
    let valid = questions.as_array().map(|a| !a.is_empty()).unwrap_or(false);
    if !valid {
        let msg = "Error: questions must be a non-empty array".to_string();
        return Ok((
            msg.clone(),
            json!({
                "status": "error",
                "input": input,
                "output": msg,
                "title": "question",
                "metadata": {},
                "time": {"start": now_ms(), "end": now_ms()},
            }),
        ));
    }
    let ask_start = now_ms();
    let qid = crate::ids::que_id();
    let request = json!({
        "id": qid,
        "sessionID": session_id,
        "questions": questions,
        "tool": {"messageID": assistant_id, "callID": call_id},
    });
    let (rx, _guard) = ctx.questions.register(&qid, request.clone());
    emit_durable(ctx, writer, session_id, "question.asked", request, seq)?;
    let outcome = ctx.questions.wait(&qid, rx).await;
    let (status, title, output, metadata) = match outcome {
        crate::question::Outcome::Answers(answers) => {
            emit_durable(
                ctx,
                writer,
                session_id,
                "question.replied",
                json!({"sessionID": session_id, "requestID": qid, "answers": answers}),
                seq,
            )?;
            (
                "completed".to_string(),
                crate::question::format_title(&questions),
                crate::question::format_output(&questions, &answers),
                json!({"answers": answers}),
            )
        }
        crate::question::Outcome::Rejected => {
            emit_durable(
                ctx,
                writer,
                session_id,
                "question.rejected",
                json!({"sessionID": session_id, "requestID": qid}),
                seq,
            )?;
            (
                "error".to_string(),
                "question".to_string(),
                crate::question::REJECTED_MESSAGE.to_string(),
                json!({}),
            )
        }
    };
    Ok((
        output.clone(),
        json!({
            "status": status,
            "input": input,
            "output": output,
            "title": title,
            "metadata": metadata,
            "time": {"start": ask_start, "end": now_ms()},
        }),
    ))
}

fn compute_cost(pricing: &Option<(f64, f64, f64)>, u: &Usage) -> f64 {
    let Some((pin, pout, pcache)) = pricing else {
        return 0.0;
    };
    // upstream getUsage: `input` EXCLUDES cache reads, which are billed
    // separately at cache_read. The provider's prompt_tokens includes cached,
    // so subtract it before applying the input rate (or cache is charged twice:
    // once at input rate, once at cache rate).
    let non_cached = u.prompt_tokens.saturating_sub(u.cached_tokens);
    (non_cached as f64 * pin + u.completion_tokens as f64 * pout + u.cached_tokens as f64 * pcache)
        / 1_000_000.0
}

#[cfg(test)]
mod sift_wiring_tests {
    use super::*;
    use serde_json::json;

    fn fake_shrink() -> std::path::PathBuf {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = std::env::temp_dir().join(format!("ocserve-sift-wire-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("shrink.sh");
        std::fs::write(
            &p,
            "#!/bin/sh\ncat >/dev/null\necho 'FAIL: assertion failed at lib.rs:42'\necho '[400 ok]'\n",
        )
        .unwrap();
        let mut perm = std::fs::metadata(&p).unwrap().permissions();
        perm.set_mode(0o755);
        std::fs::set_permissions(&p, perm).unwrap();
        p
    }

    #[test]
    fn history_rebuild_sifts_bash_but_not_read() {
        let _g = crate::sift_boundary::sift_env_lock();
        unsafe { std::env::set_var("OCSERVE_SIFT", fake_shrink().to_str().unwrap()) };
        let mut big = String::new();
        for i in 0..600 {
            big.push_str(&format!("line {i}: noise noise noise\n"));
        }
        let history = vec![(
            json!({"id": "m1", "role": "assistant"}),
            vec![
                json!({"type": "tool", "callID": "c1", "tool": "bash",
                        "state": {"output": big}}),
                json!({"type": "tool", "callID": "c2", "tool": "read",
                        "state": {"output": big}}),
            ],
        )];
        let msgs = to_provider_messages(&history);
        let results: Vec<&ChatMessage> = msgs.iter().filter(|m| m.role == "tool").collect();
        assert_eq!(results.len(), 2);
        let bash_out = results[0].content.as_str();
        let read_out = results[1].content.as_str();
        assert!(
            bash_out.contains("[compressed"),
            "bash must sift: {bash_out}"
        );
        assert!(bash_out.contains("assertion failed"), "signal kept");
        assert!(bash_out.len() <= big.len(), "never inflate");
        assert_eq!(read_out, big, "whitelist fence: read stays raw");
        unsafe { std::env::remove_var("OCSERVE_SIFT") };
    }
}

#[cfg(test)]
mod autonomy_tests {
    use super::*;

    /// 2026-10-11 bug hunt: cache tokens must not be billed at the input rate.
    /// The provider's `prompt_tokens` includes the cached portion; upstream
    /// (session.ts:361-377) bills `input = prompt − cache` at input and cache at
    /// cache_read. Charging prompt_tokens × input AND cached × cache double-bills.
    #[test]
    fn compute_cost_excludes_cache_from_input_rate() {
        let u = Usage {
            prompt_tokens: 10_000,
            completion_tokens: 1_000,
            total_tokens: 11_000,
            cached_tokens: 8_000,
            reasoning_tokens: 0,
        };
        // in=1, out=2, cache_read=0.1 USD/MTok
        let c = compute_cost(&Some((1.0, 2.0, 0.1)), &u);
        // expect: (2000×1 + 1000×2 + 8000×0.1)/1e6 = 4800/1e6
        assert!((c - 0.0048).abs() < 1e-12, "got {c}");
        // the naive (buggy) charge would have been 10_000×1 + 1000×2 + 8000×0.1
        // = 12_800 → 0.0128, i.e. 2.67× the honest cost.
    }

    #[test]
    fn auto_title_collapses_whitespace_and_caps_at_48() {
        let t =
            auto_title_from("  hello\n\t overnight   world this is a much longer first message  ")
                .unwrap();
        // byte 48 lands inside "first" → cut + trim to last space + ellipsis
        assert_eq!(t, "hello overnight world this is a much longer…");
    }

    #[test]
    fn auto_title_trims_mid_word_cuts() {
        let t = auto_title_from(&"word ".repeat(30)).unwrap();
        assert!(t.ends_with('…'), "{t:?}");
        let stem = t.strip_suffix('…').expect("ellipsis");
        assert!(!stem.ends_with(' '), "no dangling space: {t:?}");
        assert!(stem.ends_with("word"), "cut lands on a word: {t:?}");
    }

    #[test]
    fn auto_title_rejects_empty_and_whitespace() {
        assert!(auto_title_from("").is_none());
        assert!(auto_title_from("   \n\t ").is_none());
    }

    #[test]
    fn auto_title_multibyte_never_splits_a_char() {
        let t = auto_title_from(&"é".repeat(60)).unwrap();
        assert!(t.ends_with('…'), "{t:?}");
        assert!(t.contains('é'), "chars intact: {t:?}");
        // 24×2-byte chars fill exactly 48 bytes (no space → no word trim)
        assert_eq!(t.chars().filter(|c| *c == 'é').count(), 24, "{t:?}");
    }

    #[test]
    fn resolve_prefers_runopts_over_ambient() {
        let o = RunOpts {
            max_rounds: Some(7),
            cost_ceiling: Some(1.5),
            ..Default::default()
        };
        assert_eq!(resolve_max_rounds(&o, 99), 7);
        assert_eq!(resolve_cost_ceiling(&o, 99.0), 1.5);
    }

    #[test]
    fn resolve_falls_back_to_ambient_and_zero_is_unlimited() {
        let o = RunOpts::default();
        assert_eq!(resolve_max_rounds(&o, 25), 25, "ambient boot knob");
        assert_eq!(resolve_max_rounds(&o, 0), 0, "0 = unlimited (default)");
        assert_eq!(resolve_cost_ceiling(&o, 0.0), 0.0, "0 = ceiling off");
        // explicit Some(0) disables even a nonzero ambient
        let o2 = RunOpts {
            max_rounds: Some(0),
            ..Default::default()
        };
        assert_eq!(resolve_max_rounds(&o2, 50), 0);
    }
}

#[cfg(test)]
mod history_budget_tests {
    use super::*;

    fn entry(i: usize, text: String) -> (Value, Vec<Value>) {
        (
            json!({"id": format!("m{i}"), "role": if i.is_multiple_of(2) { "user" } else { "assistant" }}),
            vec![json!({"id": format!("p{i}"), "type": "text", "text": text})],
        )
    }

    #[test]
    fn small_history_is_byte_identical() {
        let h = vec![entry(0, "hello".into()), entry(1, "world".into())];
        let msgs = to_provider_messages(&h);
        assert_eq!(msgs.len(), 2, "no truncation under budget");
        assert_eq!(msgs[0].content, "hello");
        assert_eq!(msgs[1].content, "world");
    }

    #[test]
    fn over_budget_drops_oldest_first_and_keeps_newest() {
        // 9 × ~1MB > 8MB budget → oldest must go, newest must stay
        let h: Vec<_> = (0..9)
            .map(|i| entry(i, format!("msg{i} {}", "x".repeat(1024 * 1024))))
            .collect();
        let msgs = to_provider_messages(&h);
        assert!(msgs.len() < 9, "must drop oldest: kept {}", msgs.len());
        assert!(
            msgs.last().unwrap().content.starts_with("msg8 "),
            "newest exchange must survive"
        );
        assert!(
            !msgs.iter().any(|m| m.content.starts_with("msg0 ")),
            "oldest must be dropped first"
        );
        // everything kept fits the budget (slack = per-message envelope slack)
        let kept: usize = msgs.iter().map(|m| m.content.len()).sum();
        assert!(kept <= PROMPT_HISTORY_MAX_BYTES + 1024, "kept {kept} bytes");
    }

    #[test]
    fn single_oversized_message_is_always_kept() {
        // never starve the model of its only exchange
        let h = vec![entry(0, "y".repeat(PROMPT_HISTORY_MAX_BYTES + 1))];
        let msgs = to_provider_messages(&h);
        assert_eq!(msgs.len(), 1, "keep at least the newest exchange");
    }
}
