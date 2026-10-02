//! Sync prompt runner: POST /session/{id}/message semantics (captured live,
//! testdata/m2/session_fixture.json + prompt_response.json).
//!
//! Flow: persist user message → durable events + sync twins → stream provider
//! → message.part.delta frames → persist assistant message (step-start, text,
//! step-finish) → durable events → session.status/idle/diff → return {info, parts}.

use crate::event::{EventBus, frame, sync_frame};
use crate::ids::{evt_id, msg_id, prt_id};
use anyhow::{Context, Result};
use futures_util::StreamExt;
use refine_llm::{ChatMessage, Client, StreamEvent, ToolCallAssembler, Usage};
use refine_store::{BlobStore, insert_message};
use serde_json::{Value, json};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

pub struct LlmEndpoint {
    pub base_url: String,
    pub api_key: String,
    /// (input, output, cache_read) USD per MTok — None → cost 0.
    pub pricing: Option<(f64, f64, f64)>,
}

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
    ctx.bus
        .publish(frame(&ctx.directory, event_type, properties));
}

/// Run one synchronous prompt to completion.
/// Returns the assistant (info, parts) — the HTTP response body.
pub async fn run_prompt(
    ctx: &PromptContext,
    writer: &refine_store::Writer,
    session_id: &str,
    payload: &Value,
) -> Result<(Value, Vec<Value>)> {
    // 1. session must exist (friendly 404 upstream of the insert)
    if !refine_store::session_exists(&ctx.db, session_id)? {
        anyhow::bail!("Session not found: {session_id}");
    }
    let mut seq = refine_store::next_event_seq(&ctx.db, session_id)?;

    // 2. resolve model/agent from payload (agent default from ctx)
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

    // 3. persist user message (text parts only; file parts = M2b with tools)
    let user_msg_id = msg_id();
    let mut user_parts = Vec::new();
    let mut input_text = String::new();
    for p in payload["parts"]
        .as_array()
        .map(|a| a.as_slice())
        .unwrap_or(&[])
    {
        if p["type"] == "text" {
            let text = p["text"].as_str().unwrap_or("").to_string();
            input_text.push_str(&text);
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

    // 4. durable events (capture order: session.updated → message.updated → part)
    let session_info = json!({
        "id": session_id,
        "model": {"id": model, "providerID": ctx.provider_id, "variant": "default"},
        "agent": agent,
    });
    emit_durable(
        ctx,
        writer,
        session_id,
        "session.updated",
        json!({
            "sessionID": session_id, "info": session_info,
        }),
        &mut seq,
    )?;
    emit_durable(
        ctx,
        writer,
        session_id,
        "message.updated",
        json!({
            "sessionID": session_id, "info": user_info,
        }),
        &mut seq,
    )?;
    for part in &user_parts {
        emit_durable(
            ctx,
            writer,
            session_id,
            "message.part.updated",
            json!({
                "sessionID": session_id, "part": part,
            }),
            &mut seq,
        )?;
    }
    emit_live(
        ctx,
        "session.status",
        json!({
            "sessionID": session_id, "status": {"type": "busy"},
        }),
    );

    // 5. history → provider messages
    let history = refine_store::load_messages(&ctx.db, session_id)?;
    let mut messages = vec![ChatMessage {
        role: "system".into(),
        content: ctx.system.clone(),
    }];
    for (info, parts) in &history {
        let mut content = String::new();
        for p in parts {
            if p["type"] == "text" {
                content.push_str(p["text"].as_str().unwrap_or(""));
            }
        }
        if content.is_empty() {
            continue;
        }
        messages.push(ChatMessage {
            role: info["role"].as_str().unwrap_or("user").to_string(),
            content,
        });
    }

    // 6. stream
    let client = Client::new(ctx.endpoint.base_url.clone(), ctx.endpoint.api_key.clone());
    let started = Instant::now();
    let mut text = String::new();
    let mut reasoning = String::new();
    let mut finish: Option<String> = None;
    let mut usage: Option<Usage> = None;
    let mut assembler = ToolCallAssembler::default();
    let mut delta_emitted = false;

    let mut stream = client
        .chat_stream(&model, &messages, None)
        .await
        .context("provider stream open")?;
    while let Some(ev) = stream.next().await {
        match ev.context("provider stream")? {
            StreamEvent::TextDelta(t) => {
                text.push_str(&t);
                if !delta_emitted {
                    delta_emitted = true;
                }
                emit_live(
                    ctx,
                    "message.part.delta",
                    json!({
                        "sessionID": session_id,
                        "messageID": user_msg_id, // placeholder — final part ids assigned below
                        "partID": "",
                        "field": "text",
                        "delta": t,
                    }),
                );
            }
            StreamEvent::ReasoningDelta(r) => {
                reasoning.push_str(&r);
                emit_live(
                    ctx,
                    "message.part.delta",
                    json!({
                        "sessionID": session_id,
                        "messageID": user_msg_id,
                        "partID": "",
                        "field": "reasoning",
                        "delta": r,
                    }),
                );
            }
            StreamEvent::ToolCallDelta {
                index,
                id,
                name,
                arguments_delta,
            } => {
                assembler.push(index, id, name, arguments_delta);
                // tool execution = M2b (tool loop lands next)
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
    let _tool_calls = assembler.finish();
    let elapsed_ms = started.elapsed().as_millis() as i64;
    let t_done = now_ms();

    // 7. assistant message + parts (wire shape from fixture)
    let assistant_id = msg_id();
    let step_start = json!({"type": "step-start", "id": prt_id(), "sessionID": session_id, "messageID": assistant_id});
    let text_part = json!({
        "type": "text", "text": text, "time": {"start": t_done - elapsed_ms, "end": t_done},
        "id": prt_id(), "sessionID": session_id, "messageID": assistant_id,
    });
    let u = usage.clone().unwrap_or_default();
    let cost = compute_cost(&ctx.endpoint.pricing, &u);
    let finish_reason = finish.unwrap_or_else(|| "stop".into());
    let step_finish = json!({
        "reason": finish_reason,
        "type": "step-finish",
        "tokens": {
            "total": u.total_tokens, "input": u.prompt_tokens, "output": u.completion_tokens,
            "reasoning": 0,
            "cache": {"write": 0, "read": u.cached_tokens},
        },
        "cost": cost,
        "id": prt_id(), "sessionID": session_id, "messageID": assistant_id,
    });
    let assistant_info = json!({
        "parentID": user_msg_id,
        "role": "assistant",
        "mode": "primary",
        "agent": user_info["agent"],
        "path": {"cwd": ctx.directory, "root": "/"},
        "cost": cost,
        "tokens": {
            "total": u.total_tokens, "input": u.prompt_tokens, "output": u.completion_tokens,
            "reasoning": 0,
            "cache": {"write": 0, "read": u.cached_tokens},
        },
        "modelID": model,
        "providerID": ctx.provider_id,
        "time": {"created": t_done - elapsed_ms, "completed": t_done},
        "finish": finish_reason,
        "id": assistant_id,
        "sessionID": session_id,
    });
    let assistant_parts = vec![step_start.clone(), text_part.clone(), step_finish.clone()];
    insert_message(
        writer,
        Some(&*ctx.blobs),
        session_id,
        &assistant_info,
        &assistant_parts,
    )
    .context("persist assistant message")?;

    // 8. events: durable part/message/session, then live status/idle/diff
    for part in [&step_start, &text_part, &step_finish] {
        emit_durable(
            ctx,
            writer,
            session_id,
            "message.part.updated",
            json!({
                "sessionID": session_id, "part": part,
            }),
            &mut seq,
        )?;
    }
    emit_durable(
        ctx,
        writer,
        session_id,
        "message.updated",
        json!({
            "sessionID": session_id, "info": assistant_info,
        }),
        &mut seq,
    )?;
    let session_info = json!({
        "id": session_id,
        "cost": cost,
        "tokens": assistant_info["tokens"],
        "time": {"updated": t_done},
    });
    emit_durable(
        ctx,
        writer,
        session_id,
        "session.updated",
        json!({
            "sessionID": session_id, "info": session_info,
        }),
        &mut seq,
    )?;
    emit_live(
        ctx,
        "session.status",
        json!({
            "sessionID": session_id, "status": {"type": "idle"},
        }),
    );
    emit_live(
        ctx,
        "session.diff",
        json!({"sessionID": session_id, "diff": []}),
    );
    emit_live(ctx, "session.idle", json!({"sessionID": session_id}));

    // 9. update session row tokens/cost (best-effort)
    let _ = writer.write(vec![refine_store::WriteOp::Sql {
        sql: "UPDATE session SET agent = ?2, model = ?3, cost = ?4, tokens_input = ?5, \
              tokens_output = ?6, tokens_cache_read = ?7, time_updated = ?8 WHERE id = ?1"
            .into(),
        params: vec![
            session_id.into(),
            agent.into(),
            json!({"id": model, "providerID": ctx.provider_id, "variant": "default"})
                .to_string()
                .into(),
            cost.into(),
            (u.prompt_tokens as i64).into(),
            (u.completion_tokens as i64).into(),
            (u.cached_tokens as i64).into(),
            t_done.into(),
        ],
    }]);

    let _ = evt_id;
    Ok((assistant_info, assistant_parts))
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
