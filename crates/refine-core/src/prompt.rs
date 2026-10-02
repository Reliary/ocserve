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

/// Bound on provider↔tool round trips per prompt (AGENTS §2.3: bounded).
pub const MAX_STEPS: usize = 25;

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
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Emit a durable event: persist to ring, publish plain frame + sync twin.
/// `seq` is a session-local counter seeded once (avoids re-opening readers).
fn emit_durable(
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

/// Emit a non-durable live event (delta/status/idle/diff — no sync twin, per capture).
fn emit_live(ctx: &PromptContext, event_type: &str, properties: Value) {
    refine_metrics::labeled_counter(
        "refine_events_emitted_total",
        &format!("type=\"{event_type}\""),
        1,
    );
    ctx.bus
        .publish(frame(&ctx.directory, event_type, properties));
}

/// Reconstruct provider messages from stored history (text + tool parts).
fn to_provider_messages(history: &[(Value, Vec<Value>)]) -> Vec<ChatMessage> {
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
            out.push(ChatMessage::tool_result(
                p["callID"].as_str().unwrap_or_default(),
                output,
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
fn provider_stall() -> std::time::Duration {
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

pub async fn run_prompt(
    ctx: &PromptContext,
    writer: &refine_store::Writer,
    session_id: &str,
    payload: &Value,
) -> Result<(Value, Vec<Value>)> {
    if !refine_store::session_exists(&ctx.db, session_id)? {
        anyhow::bail!("Session not found: {session_id}");
    }
    let mut seq = refine_store::next_event_seq(&ctx.db, session_id)?;

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
    let user_info = json!({
        "id": user_msg_id,
        "sessionID": session_id,
        "role": "user",
        "time": {"created": t_created},
        "summary": {"diffs": []},
        "agent": agent,
        "model": {"providerID": model, "modelID": model},
    });
    insert_message(
        writer,
        Some(&*ctx.blobs),
        session_id,
        &user_info,
        &user_parts,
    )
    .context("persist user message")?;

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
    emit_live(
        ctx,
        "session.status",
        json!({"sessionID": session_id, "status": {"type": "busy"}}),
    );

    // ---- provider context ----
    let history = refine_store::load_messages(&ctx.db, session_id, None)?;
    let mut messages = vec![ChatMessage::text("system", ctx.system.clone())];
    messages.extend(to_provider_messages(&history));
    let mut tools = refine_tools::schemas();
    if let Some(hub) = &ctx.mcp {
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
    let mut total_usage = Usage::default();
    let mut total_cost = 0.0f64;
    let mut step = 0usize;

    loop {
        step += 1;
        if step > MAX_STEPS {
            anyhow::bail!("prompt exceeded {MAX_STEPS} steps (bounded loop, AGENTS §2.3)");
        }
        let started = Instant::now();
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
            client.chat_stream(&model, &messages, None, Some(&tools), Some(session_id)),
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
        let step_cost = compute_cost(&ctx.endpoint.pricing, &u);
        total_cost += step_cost;
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
                parts.push(json!({
                    "type": "text", "text": text,
                    "time": {"start": t_done - elapsed_ms, "end": t_done},
                    "id": prt_id(), "sessionID": session_id, "messageID": assistant_id,
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
                    let (rx, _perm_guard) = ctx.gate.clone().register(&perm_id, request.clone());
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
                    }
                }

                // ---- execute ----
                let exec_start = now_ms();
                let (output, meta, title, is_err) = if allowed {
                    let input = serde_json::from_str::<Value>(&call.arguments)
                        .unwrap_or_else(|_| json!({}));
                    let builtin = refine_tools::schemas()
                        .iter()
                        .any(|s| s["function"]["name"] == call.name);
                    let exec = if builtin {
                        refine_tools::execute(&call.name, &input, Path::new(&ctx.directory))
                    } else if let Some(hub) = &ctx.mcp {
                        match hub.call(&call.name, input).await {
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
                                &serde_json::from_str::<Value>(&call.arguments)
                                    .unwrap_or_else(|_| json!({})),
                                Path::new(&ctx.directory),
                            ),
                        }
                    } else {
                        refine_tools::execute(
                            &call.name,
                            &serde_json::from_str::<Value>(&call.arguments)
                                .unwrap_or_else(|_| json!({})),
                            Path::new(&ctx.directory),
                        )
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
                parts.push(running);
                provider_tool_results.push((call.id.clone(), output));
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
        parts.push(json!({
            "type": "text", "text": text, "time": {"start": t_done - elapsed_ms, "end": t_done},
            "id": prt_id(), "sessionID": session_id, "messageID": assistant_id,
        }));
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
                time_updated: t_done,
            },
        )
        .context("finalize session prompt row")?;

        return Ok((assistant_info, parts));
    }
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
