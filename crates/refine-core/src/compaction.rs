//! M6 compaction engine (COMPACTION.md §2.3/§2.4): `create` + `process`.
//!
//! Manual (`POST summarize`) and auto flows are ONE path (upstream source:
//! handlers/session.ts:273-293 = create + loop → pending task → process).
//! Divergences live in COMPACTION.md §6 (cap-3, conversation byte cap).

use crate::compact::{
    CompactionCfg, build_summary_prompt, build_summary_update_prompt, completed_compactions,
    filter_compacted, select, serialize, usable,
};
use crate::ids::{msg_id, prt_id};
use crate::prompt::{
    PromptContext, emit_durable, emit_live, hook_mutate, provider_stall, to_provider_messages,
};
use anyhow::Context as _;
use futures_util::StreamExt as _;
use refine_llm::{ChatMessage, ChatOpts, Client, StreamEvent, Usage};
use refine_store::Writer;
use serde_json::{Value, json};

/// P2 declared bound (AGENTS §2.3): conversation string cap — tail-weighted
/// (drop OLDEST entries, keep a truncation marker). Divergence D2.
pub const COMPACTION_CONVERSATION_MAX_BYTES: usize = 8 * 1024 * 1024;

/// D1 safety divergence: consecutive automatic compaction rounds per prompt
/// call; beyond this we fail honestly instead of looping (upstream has no
/// hard counter — only the summarize-overflow fail-hard).
pub const AUTO_COMPACTION_MAX_ROUNDS: u32 = 3;

/// Persist the compaction ANCHOR (v1 create, compaction.ts:559-585): a user
/// message whose only part is `{type:"compaction", auto, overflow}`.
pub async fn persist_anchor(
    ctx: &PromptContext,
    writer: &Writer,
    session_id: &str,
    agent: &str,
    auto: bool,
    overflow: bool,
) -> anyhow::Result<String> {
    let anchor_id = msg_id();
    let info = json!({
        "id": anchor_id,
        "sessionID": session_id,
        "role": "user",
        "agent": agent,
        "model": {
            "providerID": ctx.provider_id,
            "modelID": ctx.model_id,
            "variant": "default",
        },
        "time": {"created": crate::prompt::now_ms()},
    });
    let part = json!({
        "id": prt_id(),
        "sessionID": session_id,
        "messageID": anchor_id,
        "type": "compaction",
        "auto": auto,
        "overflow": overflow,
    });
    refine_store::insert_message(writer, Some(&ctx.blobs), session_id, &info, &[part])?;
    let mut seq = refine_store::next_event_seq(&ctx.db, session_id)?;
    emit_durable(
        ctx,
        writer,
        session_id,
        "message.part.updated",
        json!({"sessionID": session_id, "part": {
            "id": "", "sessionID": session_id, "messageID": anchor_id,
            "type": "compaction", "auto": auto, "overflow": overflow,
        }}),
        &mut seq,
    )?;
    emit_durable(
        ctx,
        writer,
        session_id,
        "message.updated",
        json!({"sessionID": session_id, "info": info}),
        &mut seq,
    )?;
    Ok(anchor_id)
}

fn estimate_tokens(slice: &[(Value, Vec<Value>)]) -> i64 {
    // Production estimator: serialized model-message JSON length / 4
    // (core/util/token.ts CHARS_PER_TOKEN=4). Shape = OpenAI (§17
    // divergence from upstream AI-SDK JSON — tail boundaries advisory).
    let n = serde_json::to_string(&to_provider_messages(slice))
        .map(|s| s.len())
        .unwrap_or(0);
    ((n as f64) / 4.0).round() as i64
}

/// Tail-weighted conversation builder with the declared byte bound (P2).
fn bounded_entries(entries: Vec<String>) -> Vec<String> {
    let mut out: Vec<String> = Vec::with_capacity(entries.len());
    let mut total = 0usize;
    for e in entries.iter().rev() {
        let l = e.len() + 2;
        if total + l > COMPACTION_CONVERSATION_MAX_BYTES && !out.is_empty() {
            break;
        }
        total += l;
        out.push(e.clone());
    }
    out.reverse();
    if out.len() < entries.len() {
        out.insert(
            0,
            "[earlier conversation truncated to fit the compaction bound]".to_string(),
        );
    }
    out
}

/// Request-level plugin hooks (system.transform → params → headers), same
/// semantics/order as the main loop (llm/request.ts:70/115/135).
async fn request_hooks(
    ctx: &PromptContext,
    session_id: &str,
    agent: &str,
    model: &str,
    message: &Value,
    system: &str,
    messages: &mut Vec<ChatMessage>,
) -> ChatOpts {
    let mut sys_vec = vec![system.to_string()];
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
            .filter_map(|x| x.as_str().map(str::to_string))
            .collect();
        if v.is_empty() {
            v.push(system.to_string());
        }
        if v.len() > 2 && v[0] == system {
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
    // caller built messages = [system, user(nextPrompt)] — replace index 0
    messages.splice(0..1, sys_msgs);
    let hook_in = json!({
        "sessionID": session_id,
        "agent": agent,
        "model": model,
        "provider": ctx.provider_id,
        "message": message,
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
    ChatOpts {
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
    }
}

/// The pending-compaction work (v1 processCompaction, compaction.ts:319+).
/// Returns Ok(true) = caller should continue the prompt loop (history now
/// compacted), Ok(false) = stop (overflow hard-fail persisted).
#[allow(clippy::too_many_arguments)] // flat v1-parity signature; a job struct would only rename
pub async fn process(
    ctx: &PromptContext,
    writer: &Writer,
    session_id: &str,
    cfg: &CompactionCfg,
    auto: bool,
    overflow: bool,
    anchor_id: &str,
    last_agent: &str,
) -> anyhow::Result<bool> {
    let history = refine_store::load_messages(&ctx.db, session_id, None)?;
    let rows = refine_store::compaction_rows_path(&ctx.db, session_id)?;

    // input.messages upstream = filterCompacted output (prompt.ts:1150)
    let filtered: Vec<usize> = filter_compacted(&history);
    let view: Vec<(Value, Vec<Value>)> = filtered.iter().map(|&i| history[i].clone()).collect();
    let Some(parent_pos) = view
        .iter()
        .position(|(info, _)| info["id"].as_str() == Some(anchor_id))
    else {
        anyhow::bail!("compaction anchor {anchor_id} missing from filtered history");
    };

    // overflow media-strip selection (compaction.ts:335-354)
    let mut msgs: Vec<(Value, Vec<Value>)> = view.clone();
    let mut replay: Option<(Value, Vec<Value>)> = None;
    if overflow {
        for i in (0..parent_pos).rev() {
            let (info, parts) = &view[i];
            if info["role"] == "user" && !parts.iter().any(|p| p["type"] == "compaction") {
                replay = Some(view[i].clone());
                msgs.truncate(i);
                break;
            }
        }
        let has_content = replay.is_some()
            && msgs.iter().any(|(info, parts)| {
                info["role"] == "user" && !parts.iter().any(|p| p["type"] == "compaction")
            });
        if !has_content {
            replay = None;
            msgs = view.clone();
        }
    }
    // history excludes the trailing anchor when it carries the compaction part
    if msgs
        .last()
        .map(|(info, _)| info["id"].as_str() == Some(anchor_id))
        .unwrap_or(false)
    {
        msgs.pop();
    }

    // prior completed pairs (hidden) + previous summary text
    let pairs = completed_compactions(&history, &rows);
    let hidden: std::collections::HashSet<String> = {
        let mut h = std::collections::HashSet::new();
        for (ui, si, _) in &pairs {
            if let Some(id) = history[*ui].0["id"].as_str() {
                h.insert(id.to_string());
            }
            if let Some(id) = history[*si].0["id"].as_str() {
                h.insert(id.to_string());
            }
        }
        h
    };
    let previous_summary: Option<String> = pairs.last().and_then(|(_, _, t)| t.clone());
    let sel_msgs: Vec<(Value, Vec<Value>)> = msgs
        .iter()
        .filter(|(info, _)| !hidden.contains(info["id"].as_str().unwrap_or("")))
        .cloned()
        .collect();

    // selection: head (summarize this) + tail_start_id (keep verbatim)
    let limit_context = ctx.model_limit["context"].as_i64().unwrap_or(0);
    let limit_input = ctx.model_limit["input"].as_i64().unwrap_or(0);
    let max_out = ctx.model_limit["output"].as_i64().unwrap_or(0);
    let usable_tokens = usable(cfg, limit_input, limit_context, max_out);
    let (head_len, tail_start_id) = {
        let (head, tail) = select(&sel_msgs, cfg, usable_tokens, &estimate_tokens);
        (head.len(), tail)
    };

    // messages.transform fires on the SESSION-SHAPED clone (compaction.ts:
    // 379) — this site mirrors upstream's raw shape (the main builder sends
    // OpenAI shape, §17). Fail-open: output must be the same length with
    // info/parts or the original head serializes unchanged.
    let head_vals: Vec<Value> = sel_msgs[..head_len]
        .iter()
        .map(|(info, parts)| json!({"info": info, "parts": parts}))
        .collect();
    let mt_out = hook_mutate(
        ctx.plugins.as_ref(),
        "experimental.chat.messages.transform",
        json!({}),
        json!({"messages": head_vals}),
    )
    .await;
    let head_final: Vec<(Value, Vec<Value>)> =
        match mt_out.get("messages").and_then(|a| a.as_array()) {
            Some(arr)
                if arr.len() == head_len
                    && arr
                        .iter()
                        .all(|m| m["info"].is_object() && m["parts"].is_array()) =>
            {
                arr.iter()
                    .map(|m| {
                        (
                            m["info"].clone(),
                            m["parts"].as_array().cloned().unwrap_or_default(),
                        )
                    })
                    .collect()
            }
            _ => sel_msgs[..head_len].to_vec(),
        };
    // conversation (serialized head, bounded P2)
    let head: Vec<String> = head_final
        .iter()
        .map(|(info, parts)| serialize(info, parts))
        .filter(|s| !s.is_empty())
        .collect();
    let entries = bounded_entries(head);

    // hooks: compacting (prompt ?? [built, ...context]) then the model-shaped
    // request messages get messages.transform at the builder site below
    let c_out = hook_mutate(
        ctx.plugins.as_ref(),
        "experimental.session.compacting",
        json!({"sessionID": session_id}),
        json!({"context": [], "prompt": Value::Null}),
    )
    .await;
    let custom = c_out
        .get("prompt")
        .and_then(|p| p.as_str())
        .map(str::to_string);
    let extra: Vec<String> = c_out
        .get("context")
        .and_then(|c| c.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str())
                .filter(|s| !s.is_empty())
                .map(String::from)
                .collect()
        })
        .unwrap_or_default();
    let next_prompt = match custom {
        Some(p) => p, // wholesale override (upstream prompt ?? …)
        None => {
            let built = match &previous_summary {
                Some(prev) => build_summary_update_prompt(prev, &entries),
                None => build_summary_prompt(&entries),
            };
            let mut parts = vec![built];
            parts.extend(extra);
            parts.join("\n\n")
        }
    };

    // ── summary assistant: persist shell first (stream-visible), then run ──
    let summary_id = msg_id();
    let now = crate::prompt::now_ms();
    let text_part_id = prt_id();
    let summary_info = json!({
        "id": summary_id,
        "sessionID": session_id,
        "parentID": anchor_id,
        "role": "assistant",
        "mode": "compaction",
        "agent": "compaction",
        "summary": true,
        "path": {"cwd": ctx.directory, "root": "/"},
        "cost": 0,
        "tokens": {"total": 0, "input": 0, "output": 0, "reasoning": 0,
                   "cache": {"write": 0, "read": 0}},
        "modelID": ctx.model_id,
        "providerID": ctx.provider_id,
        "time": {"created": now},
        "finish": Value::Null,
    });
    let empty_part = json!({
        "id": text_part_id,
        "sessionID": session_id,
        "messageID": summary_id,
        "type": "text",
        "text": "",
        "time": {"start": now},
    });
    refine_store::insert_message(
        writer,
        Some(&ctx.blobs),
        session_id,
        &summary_info,
        std::slice::from_ref(&empty_part),
    )?;
    let mut seq = refine_store::next_event_seq(&ctx.db, session_id)?;
    emit_durable(
        ctx,
        writer,
        session_id,
        "message.part.updated",
        json!({"sessionID": session_id, "part": empty_part}),
        &mut seq,
    )?;
    emit_durable(
        ctx,
        writer,
        session_id,
        "message.updated",
        json!({"sessionID": session_id, "info": summary_info}),
        &mut seq,
    )?;

    // ── model request (system = hidden compaction agent prompt) ──
    let mut messages = vec![
        ChatMessage::text("system", ctx.compaction_system.clone()),
        ChatMessage::text("user", next_prompt.clone()),
    ];
    let anchor_info = &history
        .iter()
        .find(|(i, _)| i["id"].as_str() == Some(anchor_id))
        .map(|(i, _)| i.clone())
        .unwrap_or_else(|| json!({}));
    let agent_label = if last_agent.is_empty() {
        "build"
    } else {
        last_agent
    };
    let copts = request_hooks(
        ctx,
        session_id,
        agent_label,
        &ctx.model_id,
        anchor_info,
        &ctx.compaction_system,
        &mut messages,
    )
    .await;
    let client = Client::new(ctx.endpoint.base_url.clone(), ctx.endpoint.api_key.clone());
    let started = std::time::Instant::now();
    let stream_res = tokio::time::timeout(
        provider_stall(),
        client.chat_stream(&ctx.model_id, &messages, &copts, None, Some(session_id)),
    )
    .await;
    let mut stream = match stream_res {
        Err(_) => anyhow::bail!(
            "compaction request stalled: no response headers within {}s (watchdog)",
            provider_stall().as_secs()
        ),
        Ok(Err(e)) => {
            if refine_llm::looks_like_context_overflow(&format!("{e:#}")) {
                return persist_overflow_error(
                    ctx,
                    writer,
                    session_id,
                    &summary_info,
                    &summary_id,
                    &text_part_id,
                );
            }
            return Err(e).context("compaction provider request");
        }
        Ok(Ok(st)) => st,
    };
    let mut text = String::new();
    let mut finish: Option<String> = None;
    let mut usage: Option<Usage> = None;
    loop {
        let ev = match tokio::time::timeout(provider_stall(), stream.next()).await {
            Err(_) => anyhow::bail!(
                "compaction stream stalled: no data for {}s (watchdog)",
                provider_stall().as_secs()
            ),
            Ok(None) => break,
            Ok(Some(ev)) => ev,
        };
        match ev.context("compaction stream")? {
            StreamEvent::TextDelta(t) => {
                emit_live(
                    ctx,
                    "message.part.delta",
                    json!({
                        "sessionID": session_id, "messageID": summary_id,
                        "partID": text_part_id, "field": "text", "delta": t,
                    }),
                );
                text.push_str(&t);
            }
            StreamEvent::ReasoningDelta(_) => {}
            StreamEvent::ToolCallDelta { .. } => {
                anyhow::bail!("compaction request unexpectedly returned tool calls");
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
    let _ = started;
    let _ = finish;
    let _ = usage; // upstream zeroes summary tokens (compaction.ts:410-415)

    // summary text part update + events
    let done = crate::prompt::now_ms();
    let final_part = json!({
        "id": text_part_id,
        "sessionID": session_id,
        "messageID": summary_id,
        "type": "text",
        "text": text,
        "time": {"start": now, "end": done},
    });
    refine_store::update_part(
        writer,
        &ctx.blobs,
        session_id,
        &summary_id,
        &text_part_id,
        &final_part,
    )?;
    emit_durable(
        ctx,
        writer,
        session_id,
        "message.part.updated",
        json!({"sessionID": session_id, "part": final_part}),
        &mut seq,
    )?;
    let finalized_info = json!({
        "id": summary_id,
        "sessionID": session_id,
        "parentID": anchor_id,
        "role": "assistant",
        "mode": "compaction",
        "agent": "compaction",
        "summary": true,
        "path": {"cwd": ctx.directory, "root": "/"},
        "cost": 0,
        "tokens": {"total": 0, "input": 0, "output": 0, "reasoning": 0,
                   "cache": {"write": 0, "read": 0}},
        "modelID": ctx.model_id,
        "providerID": ctx.provider_id,
        "time": {"created": now, "completed": done},
        "finish": "stop",
    });
    refine_store::update_message_info(writer, session_id, &summary_id, &finalized_info)?;
    emit_durable(
        ctx,
        writer,
        session_id,
        "message.updated",
        json!({"sessionID": session_id, "info": finalized_info}),
        &mut seq,
    )?;

    // tail_start_id on the anchor's compaction part (compaction.ts:460-466)
    if let Some(tail) = &tail_start_id {
        let anchor_parts = history
            .iter()
            .find(|(i, _)| i["id"].as_str() == Some(anchor_id))
            .map(|(_, p)| p.clone())
            .unwrap_or_default();
        if let Some(part) = anchor_parts.iter().find(|p| p["type"] == "compaction") {
            let cur = part["tail_start_id"].as_str().unwrap_or("");
            if cur != tail.as_str() {
                let mut updated = part.clone();
                updated["tail_start_id"] = json!(tail);
                refine_store::update_part(
                    writer,
                    &ctx.blobs,
                    session_id,
                    anchor_id,
                    part["id"].as_str().unwrap_or(""),
                    &updated,
                )?;
                emit_durable(
                    ctx,
                    writer,
                    session_id,
                    "message.part.updated",
                    json!({"sessionID": session_id, "part": updated}),
                    &mut seq,
                )?;
            }
        }
    }

    // continue block (compaction.ts:505-545): overflow→replay clone,
    // otherwise autocontinue message; manual (auto=false) → neither
    if auto {
        if let Some((rinfo, rparts)) = replay {
            let mut new_info = rinfo;
            let new_id = msg_id();
            new_info["id"] = json!(new_id);
            new_info["sessionID"] = json!(session_id);
            new_info["time"] = json!({"created": crate::prompt::now_ms()});
            let mut new_parts: Vec<Value> = Vec::with_capacity(rparts.len());
            for p in &rparts {
                if p["type"] == "compaction" {
                    continue;
                }
                let mut np = if p["type"] == "file" {
                    json!({
                        "id": prt_id(), "type": "text",
                        "text": format!(
                            "[Attached {}: {}]",
                            p["mime"].as_str().unwrap_or("file"),
                            p["filename"].as_str().unwrap_or("file")
                        ),
                    })
                } else {
                    p.clone()
                };
                np["id"] = json!(prt_id());
                np["sessionID"] = json!(session_id);
                np["messageID"] = json!(new_id);
                new_parts.push(np);
            }
            refine_store::insert_message(
                writer,
                Some(&ctx.blobs),
                session_id,
                &new_info,
                &new_parts,
            )?;
            emit_durable(
                ctx,
                writer,
                session_id,
                "message.updated",
                json!({"sessionID": session_id, "info": new_info}),
                &mut seq,
            )?;
            for p in &new_parts {
                emit_durable(
                    ctx,
                    writer,
                    session_id,
                    "message.part.updated",
                    json!({"sessionID": session_id, "part": p}),
                    &mut seq,
                )?;
            }
        } else {
            let cmsg_id = msg_id();
            let cinfo = json!({
                "id": cmsg_id,
                "sessionID": session_id,
                "role": "user",
                "agent": last_agent,
                "time": {"created": crate::prompt::now_ms()},
            });
            let prefix = if overflow {
                "The previous request exceeded the provider's size limit due to large media attachments. The conversation was compacted and media files were removed from context. If the user was asking about attached images or files, explain that the attachments were too large to process and suggest they try again with smaller or fewer files.\n\n"
            } else {
                ""
            };
            let ctext = json!({
                "id": prt_id(),
                "sessionID": session_id,
                "messageID": cmsg_id,
                "type": "text",
                "text": format!("{prefix}Continue if you have next steps, or stop and ask for clarification if you are unsure how to proceed."),
                "metadata": {"compaction_continue": true},
            });
            refine_store::insert_message(
                writer,
                Some(&ctx.blobs),
                session_id,
                &cinfo,
                std::slice::from_ref(&ctext),
            )?;
            emit_durable(
                ctx,
                writer,
                session_id,
                "message.part.updated",
                json!({"sessionID": session_id, "part": ctext}),
                &mut seq,
            )?;
            emit_durable(
                ctx,
                writer,
                session_id,
                "message.updated",
                json!({"sessionID": session_id, "info": cinfo}),
                &mut seq,
            )?;
        }
    }

    // prune (compaction.ts:275-318): timestamps only, over already-loaded history
    if cfg.prune {
        prune_tool_outputs(ctx, writer, session_id, &history, &mut seq)?;
    }

    emit_live(ctx, "session.compacted", json!({"sessionID": session_id}));
    Ok(true)
}

/// Honest hard-fail when the summarize request itself overflows
/// (compaction.ts:450-457 ContextOverflowError) — persist error on the
/// summary assistant, publish session.error, tell caller to stop.
fn persist_overflow_error(
    ctx: &PromptContext,
    writer: &Writer,
    session_id: &str,
    summary_info: &Value,
    summary_id: &str,
    text_part_id: &str,
) -> anyhow::Result<bool> {
    let done = crate::prompt::now_ms();
    let mut info = summary_info.clone();
    info["finish"] = json!("error");
    info["time"] = json!({"created": info["time"]["created"], "completed": done});
    info["error"] = json!({
        "type": "ContextOverflowError",
        "data": {"message": "Conversation history too large to compact - context exceeds model limit even after stripping media"}
    });
    let err_part = json!({
        "id": text_part_id,
        "sessionID": session_id,
        "messageID": summary_id,
        "type": "text",
        "text": "",
        "time": {"start": done, "end": done},
    });
    refine_store::insert_message(
        writer,
        Some(&ctx.blobs),
        session_id,
        &info,
        std::slice::from_ref(&err_part),
    )?;
    let mut seq = refine_store::next_event_seq(&ctx.db, session_id)?;
    emit_durable(
        ctx,
        writer,
        session_id,
        "message.part.updated",
        json!({"sessionID": session_id, "part": err_part}),
        &mut seq,
    )?;
    emit_durable(
        ctx,
        writer,
        session_id,
        "message.updated",
        json!({"sessionID": session_id, "info": info}),
        &mut seq,
    )?;
    emit_live(
        ctx,
        "session.error",
        json!({"sessionID": session_id, "error": info["error"]}),
    );
    Ok(false)
}

/// prune port: walk backward from newest, keep last PRUNE_PROTECT tokens of
/// completed tool outputs, mark older ones with time.compacted (never delete).
fn prune_tool_outputs(
    ctx: &PromptContext,
    writer: &Writer,
    session_id: &str,
    history: &[(Value, Vec<Value>)],
    seq: &mut i64,
) -> anyhow::Result<()> {
    const PRUNE_MINIMUM: i64 = 20_000;
    const PRUNE_PROTECT: i64 = 40_000;
    let mut total = 0i64;
    let mut pruned = 0i64;
    let mut turns = 0i64;
    let mut to_mark: Vec<(String, Value)> = Vec::new(); // (part_id, updated_part)
    'outer: for (info, parts) in history.iter().rev() {
        if info["role"] == "user" {
            turns += 1;
        }
        if turns < 2 {
            continue;
        }
        if info["role"] == "assistant" && info["summary"] == Value::Bool(true) {
            break;
        }
        for part in parts.iter().rev() {
            if part["type"] != "tool" || part["state"]["status"] != "completed" {
                continue;
            }
            let tool = part["tool"].as_str().unwrap_or("");
            if tool == "skill" {
                continue;
            }
            if !part["state"]["time"]["compacted"].is_null() {
                break 'outer;
            }
            let output = part["state"]["output"].as_str().unwrap_or("");
            let est = (output.len() as f64 / 4.0).round() as i64;
            total += est;
            if total <= PRUNE_PROTECT {
                continue;
            }
            pruned += est;
            let mut updated = part.clone();
            updated["state"]["time"]["compacted"] = json!(crate::prompt::now_ms());
            if let Some(pid) = part["id"].as_str() {
                to_mark.push((pid.to_string(), updated));
            }
        }
    }
    if pruned > PRUNE_MINIMUM {
        for (pid, updated) in to_mark {
            refine_store::update_part(
                writer,
                &ctx.blobs,
                session_id,
                updated["messageID"].as_str().unwrap_or(""),
                &pid,
                &updated,
            )?;
            emit_durable(
                ctx,
                writer,
                session_id,
                "message.part.updated",
                json!({"sessionID": session_id, "part": updated}),
                seq,
            )?;
        }
    }
    Ok(())
}

// re-export for trigger math at prompt sites
pub use crate::compact::is_overflow as overflow_at;
