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
use refine_llm::{ChatMessage, Client, StreamEvent, ToolCallAssembler, Usage};
use refine_store::{BlobStore, insert_message};
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
    pub rules: Vec<refine_tools::Rule>,
    /// Permission rendezvous (ask → event → reply).
    pub gate: Arc<PermissionGate>,
    /// MCP hub (M4a): namespaced tools merged into the provider tool list.
    pub mcp: Option<Arc<refine_mcp::McpHub>>,
    /// Plugin sidecar (M4b): hook dispatch (v1 names, e.g. tool.execute.after).
    pub plugins: Option<Arc<tokio::sync::Mutex<refine_plugin::Sidecar>>>,
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

/// Emit a durable event: persist to ring, publish plain frame + sync twin.
/// `seq` is a session-local counter seeded once (avoids re-opening readers).
pub(crate) fn emit_durable(
    ctx: &PromptContext,
    writer: &refine_store::Writer,
    session_id: &str,
    event_type: &str,
    properties: Value,
    seq: &mut i64,
) -> Result<()> {
    refine_store::append_event(writer, Some(session_id), event_type, &properties)?;
    refine_metrics::labeled_counter(
        "refine_events_emitted_total",
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

/// Loop-guard permission rendezvous (D1/P1b): same durable event shape as
/// the normal permission ask but `action="doom_loop"` (upstream's permission
/// name, processor.ts:373) with additive metadata.class. Returns true when
/// the user approved once/always.
#[allow(clippy::too_many_arguments)]
async fn loop_guard_ask(
    ctx: &PromptContext,
    writer: &refine_store::Writer,
    session_id: &str,
    class: crate::loop_guard::Class,
    tool: &str,
    input_key: &str,
    message_id: &str,
    call_id: &str,
    seq: &mut i64,
) -> Result<bool> {
    let perm_id = crate::ids::evt_id();
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
        json!({"sessionID": session_id, "requestID": perm_id}),
        seq,
    )?;
    if reply == "always" {
        let key = format!("doom_loop:{tool}");
        ctx.gate.grant_always(session_id, &key);
        // K-ALWAYS: persist past restart (memory stays the fast path)
        if let Err(e) = refine_store::session_grant_always(writer, &ctx.db, session_id, &key) {
            tracing::error!("persist always grant {key}: {e:#}");
        }
    }
    Ok(reply == "once" || reply == "always")
}

/// Trigger a plugin hook with v1 in-place mutation semantics: returns the
/// (possibly mutated) `output`. Fail-open by construction — no sidecar or a
/// hook error returns `output` unchanged (a broken plugin never breaks a
/// prompt; M4b rule). Metrics: duration + error counter per hook name.
pub async fn hook_mutate(
    plugins: Option<&Arc<tokio::sync::Mutex<refine_plugin::Sidecar>>>,
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
            refine_metrics::observe(
                "refine_plugin_hook_duration_seconds",
                &format!("hook=\"{name}\",result=\"ok\""),
                t0.elapsed().as_micros() as u64,
            );
            v
        }
        Err(e) => {
            refine_metrics::observe(
                "refine_plugin_hook_duration_seconds",
                &format!("hook=\"{name}\",result=\"error\""),
                t0.elapsed().as_micros() as u64,
            );
            refine_metrics::labeled_counter(
                "refine_plugin_hook_errors_total",
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
    refine_metrics::labeled_counter(
        "refine_events_emitted_total",
        &format!("type=\"{event_type}\""),
        1,
    );
    ctx.bus
        .publish(frame(&ctx.directory, event_type, properties));
}

/// Reconstruct provider messages from stored history (text + tool parts).
pub(crate) fn to_provider_messages(history: &[(Value, Vec<Value>)]) -> Vec<ChatMessage> {
    let mut out = Vec::new();
    for (info, parts) in history {
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
/// REFINE_PROVIDER_STALL_SECS=1); production default 120s — deltas keep
/// resetting it, so only a truly silent socket trips it.
pub(crate) fn provider_stall() -> std::time::Duration {
    static S: std::sync::OnceLock<std::time::Duration> = std::sync::OnceLock::new();
    *S.get_or_init(|| {
        std::time::Duration::from_secs(
            std::env::var("REFINE_PROVIDER_STALL_SECS")
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
            rss0: refine_metrics::rss_bytes(),
            peak: refine_metrics::rss_bytes(),
            opens0: refine_store::pragma::reader_opens(),
        }
    }
    fn sample_round(&mut self) {
        if let Some(now) = refine_metrics::rss_bytes() {
            self.peak = Some(self.peak.map_or(now, |p: i64| p.max(now)));
        }
    }
}

impl Drop for PhaseProbe {
    fn drop(&mut self) {
        refine_metrics::counter(
            "refine_db_opens_total",
            refine_store::pragma::reader_opens().saturating_sub(self.opens0),
        );
        if let (Some(start), Some(peak)) = (self.rss0, self.peak) {
            refine_metrics::gauge("refine_prompt_rss_start_bytes", start);
            refine_metrics::gauge("refine_prompt_rss_delta_bytes", (peak - start).max(0));
        }
    }
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
    /// REFINE_PROMPT_MAX_ROUNDS; 0/absent = unlimited — K-AUTONOMY:
    /// sessions must run as long as they need, including overnight)
    pub max_rounds: Option<usize>,
    /// hard USD ceiling for this run (None → env REFINE_PROMPT_MAX_COST_USD;
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
    writer: &refine_store::Writer,
    session_id: &str,
    payload: &Value,
) -> Result<(Value, Vec<Value>)> {
    let res = run_prompt_with(ctx, writer, session_id, payload, RunOpts::default()).await;
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
    writer: &refine_store::Writer,
    session_id: &str,
    payload: &Value,
    opts: RunOpts,
) -> Result<(Value, Vec<Value>)> {
    if !refine_store::session_exists(&ctx.db, session_id)? {
        anyhow::bail!("Session not found: {session_id}");
    }
    // K-ALWAYS: one DB read pulls this session's persisted always-grants
    // into the gate (memory-first consult afterwards).
    ctx.gate.hydrate(&ctx.db, session_id)?;
    // K-MODEL-STATE: resolved model+agent land at START (not finalize) —
    // cap/OOM/abort deaths keep the selection sticky for the next
    // payload-less send. Churn-free UPDATE (0 rows when unchanged).
    refine_store::persist_prompt_model(
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
    let mut pre = refine_store::compaction_preflight(&ctx.db, session_id)?;
    let mut seq = pre.seq;
    let mut pre_dirty = false;

    let model = payload
        .pointer("/model/modelID")
        .and_then(|v| v.as_str())
        .unwrap_or(&ctx.model_id)
        .to_string();
    let agent = payload
        .get("agent")
        .and_then(|v| v.as_str())
        .filter(|a| !a.is_empty())
        .unwrap_or(&ctx.agent)
        .to_string();

    // ---- persist user message (text parts; file parts land with M3) ----
    let user_msg_id = match payload.get("messageId").and_then(|v| v.as_str()) {
        Some(id) if id.len() <= 64 && id.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') => {
            id.to_string()
        }
        _ => msg_id(),
    };
    let mut user_parts = Vec::new();
    for p in payload["parts"]
        .as_array()
        .map(|a| a.as_slice())
        .unwrap_or(&[])
    {
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
    // v1 parity: chat.message (prompt.ts:1000) fires BEFORE persistence so
    // plugins (magic-context) can mutate {message, parts} into history.
    // Fidelity note: only the REAL prompt flow fires it — upstream's
    // summarize (compaction.ts) persists its user marker through a
    // different path and never triggers chat.message. persist_user alone
    // can't distinguish (refine summarize persists the marker too, probe
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
                "variant": payload.get("variant").cloned().unwrap_or(Value::Null),
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

    emit_durable(
        ctx,
        writer,
        session_id,
        "session.updated",
        json!({
            "sessionID": session_id,
            "info": {
                "id": session_id,
                "model": {"id": model, "providerID": ctx.provider_id, "variant": "default"},
                "agent": agent,
            },
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
    emit_live(
        ctx,
        "session.status",
        json!({"sessionID": session_id, "status": {"type": "busy"}}),
    );

    // ── M6 outer compaction loop (upstream runLoop, prompt.ts:1083+) ──
    // Rounds re-filter history, run the pending engine step, then rebuild
    // messages; D1 cap counts engine rounds (COMPACTION §6).
    let mut total_usage = Usage::default();
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
            pre = refine_store::compaction_preflight(&ctx.db, session_id)?;
            seq = pre.seq;
        }
        // loop-guard window: resets per compaction round (each round is a
        // fresh provider conversation) — DIFFERENTIATION D1
        let mut loop_win: Vec<crate::loop_guard::Entry> = Vec::new();
        match std::mem::replace(&mut pre.state, refine_store::PreflightState::Ready) {
            refine_store::PreflightState::Pending {
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
            refine_store::PreflightState::SummaryExit { info, parts } => {
                final_out = Some((info, parts));
                break 'outer;
            }
            refine_store::PreflightState::Ready => {}
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
            let history = refine_store::load_messages(&ctx.db, session_id, None)?;
            messages.extend(to_provider_messages(&history));
        }
        let mut tools = if opts.tools_enabled {
            refine_tools::schemas()
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
                .map(|n| refine_tools::evaluate(n, "*", &ctx.rules) != "deny")
                .unwrap_or(true)
        });

        let client = Client::new(ctx.endpoint.base_url.clone(), ctx.endpoint.api_key.clone());
        // leading system messages in `messages` (starts with the single
        // pre-built system message; system.transform may reshape the prefix)
        let mut sys_count: usize = 1;
        // v1 parity: experimental.chat.messages.transform — prompt.ts:1255 (main
        // inference) AND compaction.ts:379 (summarize): refine funnels both
        // flows through this builder, so ONE site covers both upstream sites.
        // The system prefix is NOT exposed (upstream fires on the stored
        // conversation; system is built separately there). Live-request only —
        // DB history untouched by construction. Fail-open on shape mismatch.
        // Shape note (§17): OpenAI messages (refine's wire), documented
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
                    "turn exceeded {max_rounds} rounds (REFINE_PROMPT_MAX_ROUNDS — raise it or unset to disable)"
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
            let copts = refine_llm::ChatOpts {
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
            };
            let mut llm_ttft: Option<std::time::Duration> = None;
            let mut text = String::new();
            let mut reasoning = String::new();
            let mut finish: Option<String> = None;
            let mut usage: Option<Usage> = None;
            let mut assembler = ToolCallAssembler::default();

            // Watchdog covers BOTH hang points: response headers (.send inside
            // chat_stream) and the read loop below — a silent socket at either
            // stage must fail, never hang busy (A3).
            let mut stream = match tokio::time::timeout(
                provider_stall(),
                client.chat_stream(
                    &model,
                    &messages,
                    &copts,
                    if opts.tools_enabled {
                        Some(&tools)
                    } else {
                        None
                    },
                    Some(session_id),
                ),
            )
            .await
            {
                Err(_) => {
                    refine_metrics::labeled_counter(
                        "refine_llm_stream_errors_total",
                        &format!("provider=\"{}\",model=\"{model}\"", ctx.provider_id),
                        1,
                    );
                    anyhow::bail!(
                        "provider stream open stalled: no response headers within {}s (watchdog)",
                        provider_stall().as_secs()
                    );
                }
                Ok(Err(e)) => {
                    refine_metrics::labeled_counter(
                        "refine_llm_stream_errors_total",
                        &format!("provider=\"{}\",model=\"{model}\"", ctx.provider_id),
                        1,
                    );
                    // M6 overflow class (upstream processor.ts:624-634): a
                    // size-rejection at request open becomes a pending anchor
                    // with overflow=true, then the outer loop re-filters.
                    let emsg = format!("{e:#}");
                    if refine_llm::looks_like_context_overflow(&emsg) {
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
                        }
                        emit_live(
                            ctx,
                            "message.part.delta",
                            json!({
                                "sessionID": session_id, "messageID": user_msg_id, "partID": "",
                                "field": "text", "delta": t,
                            }),
                        );
                        text.push_str(&t);
                    }
                    StreamEvent::ReasoningDelta(r) => {
                        emit_live(
                            ctx,
                            "message.part.delta",
                            json!({
                                "sessionID": session_id, "messageID": user_msg_id, "partID": "",
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
                refine_metrics::observe(
                    "refine_llm_request_duration_seconds",
                    &llm_label,
                    started.elapsed().as_micros() as u64,
                );
                if let Some(ttft) = llm_ttft {
                    refine_metrics::observe(
                        "refine_llm_ttft_seconds",
                        &llm_label,
                        ttft.as_micros() as u64,
                    );
                }
            }
            let t_done = now_ms();
            let finish_reason = finish.unwrap_or_else(|| "stop".into());
            let u = usage.clone().unwrap_or_default();
            total_usage.prompt_tokens += u.prompt_tokens;
            total_usage.completion_tokens += u.completion_tokens;
            total_usage.total_tokens += u.total_tokens;
            total_usage.cached_tokens += u.cached_tokens;
            if u.cached_tokens > 0 {
                refine_metrics::labeled_counter(
                    "refine_llm_cache_tokens_total",
                    &format!("provider=\"{}\",dir=\"read\"", ctx.provider_id),
                    u.cached_tokens,
                );
            }
            let step_cost = compute_cost(&ctx.endpoint.pricing, &u);
            total_cost += step_cost;
            if cost_ceiling > 0.0 && total_cost >= cost_ceiling {
                death = Some(format!(
                    "prompt reached the cost ceiling ${cost_ceiling:.4} (REFINE_PROMPT_MAX_COST_USD)"
                ));
                break 'outer;
            }
            let assistant_id = msg_id();

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
                        "id": prt_id(), "sessionID": session_id, "messageID": assistant_id,
                    }));
                }
                if !text.is_empty() {
                    let text_part_id = prt_id();
                    // v1 parity: experimental.text.complete fires at text-end
                    // BEFORE the part persists (processor.ts:531); mutation
                    // flows into persistence AND the provider continuation below.
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
                    let resource = permission_resource(&call.name, &call.arguments);
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
                            refine_metrics::observe(
                                "refine_plugin_hook_duration_seconds",
                                "hook=\"tool.execute.after\",result=\"ok\"",
                                hook_t0.elapsed().as_micros() as u64,
                            );
                            if let Err(e) = &hook_res {
                                refine_metrics::labeled_counter(
                                    "refine_plugin_hook_errors_total",
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

                    // ---- permission gate ----
                    let key = format!("{}:{}", call.name, resource);
                    let effect = if ctx.gate.check_always(session_id, &key) {
                        "allow".to_string()
                    } else {
                        refine_tools::evaluate(&call.name, &resource, &ctx.rules)
                    };
                    let mut allowed = effect == "allow";
                    // hoisted so the trust observe below (post-branch) can see it
                    let builtin = refine_tools::schemas()
                        .iter()
                        .any(|s| s["function"]["name"] == call.name);
                    if effect == "ask" {
                        let perm_id = crate::ids::evt_id(); // 26-char request id
                        let request = json!({
                            "id": perm_id,
                            "sessionID": session_id,
                            "action": call.name,
                            "resource": resource,
                            "patterns": [resource],
                            "always": [resource],
                            "metadata": {},
                            "tool": {"messageID": assistant_id, "callID": call.id},
                        });
                        let (rx, _perm_guard) =
                            ctx.gate.clone().register(&perm_id, request.clone());
                        emit_durable(
                            ctx,
                            writer,
                            session_id,
                            "permission.asked",
                            json!({
                                "sessionID": session_id,
                                "id": perm_id,
                                "permission": call.name,
                                "patterns": [resource],
                                "always": [resource],
                                "metadata": {},
                                "tool": {"messageID": assistant_id, "callID": call.id},
                            }),
                            &mut seq,
                        )?;
                        let reply = ctx.gate.wait(&perm_id, rx).await;
                        emit_durable(
                            ctx,
                            writer,
                            session_id,
                            "permission.replied",
                            json!({"sessionID": session_id, "requestID": perm_id}),
                            &mut seq,
                        )?;
                        allowed = reply == "once" || reply == "always";
                        if reply == "always" {
                            ctx.gate.grant_always(session_id, &key);
                            // K-ALWAYS: persist past restart
                            if let Err(e) = refine_store::session_grant_always(
                                writer, &ctx.db, session_id, &key,
                            ) {
                                tracing::error!("persist always grant {key}: {e:#}");
                            }
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
                            refine_metrics::labeled_counter(
                                "refine_agent_health_total",
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
                            refine_metrics::labeled_counter(
                                "refine_agent_health_action_total",
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
                        let exec = if builtin {
                            refine_tools::execute(&call.name, &input, Path::new(&ctx.directory))
                        } else if let Some(hub) = &ctx.mcp {
                            match hub.call(&call.name, input.clone()).await {
                                Some(Ok(text)) => Ok(refine_tools::ToolResult {
                                    output: text,
                                    truncated: false,
                                    exit: None,
                                    title: call.name.clone(),
                                    error: false,
                                    metadata: None,
                                }),
                                Some(Err(e)) => Err(e),
                                None => refine_tools::execute(
                                    &call.name,
                                    &input,
                                    Path::new(&ctx.directory),
                                ),
                            }
                        } else {
                            refine_tools::execute(&call.name, &input, Path::new(&ctx.directory))
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
                        output = refine_mcp::McpHub::guard_output(&call.name, output);
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
                        let mut ops = vec![refine_store::WriteOp::Sql {
                            sql: "DELETE FROM todo WHERE session_id = ?1".into(),
                            params: vec![session_id.into()],
                        }];
                        for (i, t) in todos.iter().enumerate() {
                            ops.push(refine_store::WriteOp::Sql {
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
                        refine_metrics::observe(
                            "refine_plugin_hook_duration_seconds",
                            "hook=\"tool.execute.after\",result=\"ok\"",
                            hook_t0.elapsed().as_micros() as u64,
                        );
                        if let Err(e) = &hook_res {
                            refine_metrics::labeled_counter(
                                "refine_plugin_hook_errors_total",
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
                        refine_metrics::labeled_counter(
                            "refine_agent_health_total",
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
                            refine_metrics::labeled_counter(
                                "refine_agent_health_action_total",
                                if ok {
                                    "action=\"approved\""
                                } else {
                                    "action=\"denied\""
                                },
                                1,
                            );
                            if !ok {
                                anyhow::bail!(
                                    "loop guard: {} detected and declined by user (set REFINE_LOOP_GUARD=0 to disable asks)",
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
                    "tokens": {
                        "total": u.total_tokens, "input": u.prompt_tokens,
                        "output": u.completion_tokens, "reasoning": 0,
                        "cache": {"write": 0, "read": u.cached_tokens},
                    },
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
                emit_durable(
                    ctx,
                    writer,
                    session_id,
                    "session.updated",
                    json!({
                        "sessionID": session_id,
                        "info": {"id": session_id, "cost": total_cost, "time": {"updated": t_done}},
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
                    "id": prt_id(), "sessionID": session_id, "messageID": assistant_id,
                }));
            }
            if !text.is_empty() {
                let text_part_id = prt_id();
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
                "cost": total_cost,
                "tokens": {
                    "total": total_usage.total_tokens, "input": total_usage.prompt_tokens,
                    "output": total_usage.completion_tokens, "reasoning": 0,
                    "cache": {"write": 0, "read": total_usage.cached_tokens},
                },
                "modelID": model,
                "providerID": ctx.provider_id,
                "time": {"created": t_done - elapsed_ms, "completed": t_done},
                "finish": finish_reason,
                "id": assistant_id,
                "sessionID": session_id,
            });
            parts.push(json!({
                "reason": finish_reason, "type": "step-finish",
                "tokens": assistant_info["tokens"], "cost": total_cost,
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
            emit_durable(
                ctx,
                writer,
                session_id,
                "session.updated",
                json!({
                    "sessionID": session_id,
                    "info": {
                        "id": session_id,
                        "cost": total_cost,
                        "tokens": assistant_info["tokens"],
                        "time": {"updated": t_done},
                    },
                }),
                &mut seq,
            )?;
            // ── M6 post-turn trigger (upstream prompt.ts:1160-1167): the just-
            // finished assistant's tokens vs usable → pending anchor; at the D1
            // cap emit the honest error and stop compacting (answer is kept).
            let a_total = assistant_info["tokens"]["total"].as_i64().unwrap_or(0);
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
    refine_store::finalize_session_prompt(
        writer,
        session_id,
        &refine_store::PromptStats {
            agent: &agent,
            model_json: &json!({
                "id": model,
                "providerID": ctx.provider_id,
                "variant": "default"
            })
            .to_string(),
            cost: total_cost,
            tokens_input: total_usage.prompt_tokens,
            tokens_output: total_usage.completion_tokens,
            tokens_cache_read: total_usage.cached_tokens,
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
/// REFINE_PROMPT_MAX_ROUNDS, 0 = unlimited). Pure — unit-tested without
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
    refine_metrics::labeled_counter(
        "refine_prompt_rounds_total",
        &format!("bucket=\"{bucket}\",finish=\"{finish}\""),
        1,
    );
}

/// D-TITLE-1 (refine-only): derive a session title from its first user
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
fn auto_retag(ctx: &PromptContext, writer: &refine_store::Writer, sid: &str) {
    let text = match refine_store::first_user_text(&ctx.db, sid, Some(&*ctx.blobs)) {
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
    match refine_store::retag_default_title(writer, sid, &title) {
        Ok(true) => {
            ctx.bus.publish(frame(
                &ctx.directory,
                "session.updated",
                json!({"sessionID": sid, "info": {"id": sid, "title": title}}),
            ));
        }
        Ok(false) => {} // named by the user already — never overwrite
        Err(e) => tracing::warn!("auto-title retag failed: {e:#}"),
    }
}

/// K-AUTONOMY surfacing for EVERY async/sync run failure (the "just
/// stopped, no error" class): durable `[turn stopped]` part on the last
/// assistant message (+ finalize when it was incomplete), durable
/// session.error (oc-remote toast — shape matches the proven
/// emit_session_error_event: name + message + data.message), live idle trio
/// so status consumers settle (may repeat the post-loop emits on
/// break-path deaths — idempotent events).
async fn surface_run_failure(
    ctx: &PromptContext,
    writer: &refine_store::Writer,
    sid: &str,
    err: &anyhow::Error,
) {
    let full = format!("{err:#}");
    let short: String = full.chars().take(2000).collect();
    let pid = crate::ids::prt_id();
    match refine_store::mark_turn_stopped(writer, &ctx.db, sid, &pid, &short) {
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
    let mut eseq = refine_store::next_event_seq(&ctx.db, sid).unwrap_or(1);
    if let Err(e) = emit_durable(
        ctx,
        writer,
        sid,
        "session.error",
        json!({
            "sessionID": sid,
            "error": {"name": "UnknownError", "message": short,
                      "data": {"message": short}}
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
    writer: &refine_store::Writer,
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

/// Permission resource per tool (v1: fs tools → path, bash → command).
fn permission_resource(tool: &str, arguments: &str) -> String {
    let Ok(v) = serde_json::from_str::<Value>(arguments) else {
        return "*".to_string();
    };
    match tool {
        "bash" => v["command"].as_str().unwrap_or("*").to_string(),
        "read" | "write" | "edit" => v["filePath"].as_str().unwrap_or("*").to_string(),
        "glob" | "grep" => v["path"]
            .as_str()
            .or_else(|| v["pattern"].as_str())
            .unwrap_or("*")
            .to_string(),
        _ => "*".to_string(),
    }
}

fn compute_cost(pricing: &Option<(f64, f64, f64)>, u: &Usage) -> f64 {
    let Some((pin, pout, pcache)) = pricing else {
        return 0.0;
    };
    (u.prompt_tokens as f64 * pin
        + u.completion_tokens as f64 * pout
        + u.cached_tokens as f64 * pcache)
        / 1_000_000.0
}

#[cfg(test)]
mod sift_wiring_tests {
    use super::*;
    use serde_json::json;

    fn fake_shrink() -> std::path::PathBuf {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = std::env::temp_dir().join(format!("refine-sift-wire-{}", std::process::id()));
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
        unsafe { std::env::set_var("REFINE_SIFT", fake_shrink().to_str().unwrap()) };
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
        unsafe { std::env::remove_var("REFINE_SIFT") };
    }
}

#[cfg(test)]
mod autonomy_tests {
    use super::*;

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
