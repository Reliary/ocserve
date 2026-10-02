//! refine-llm: provider clients. M2 scope: openai-compatible streaming with
//! tool-call delta assembly and usage accounting.
//!
//! Contract sources: real recorded streams in `testdata/m2/llm_stream_*.bin`
//! (deepseek official API — reasoning deltas, finish=stop|length, usage with
//! prompt_cache_hit/miss fields).

use anyhow::{Context, Result};
use futures_util::StreamExt;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatMessage {
    pub role: String,
    pub content: String,
}

#[derive(Debug, Clone, Default)]
pub struct Usage {
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    pub total_tokens: u64,
    pub cached_tokens: u64,
}

/// Provider stream events (parsed from OpenAI-compatible SSE `data:` lines).
#[derive(Debug, Clone)]
pub enum StreamEvent {
    ReasoningDelta(String),
    TextDelta(String),
    ToolCallDelta {
        index: usize,
        id: Option<String>,
        name: Option<String>,
        arguments_delta: String,
    },
    Done {
        finish: Option<String>,
        usage: Option<Usage>,
    },
}

/// Assembled tool call after all deltas are consumed.
#[derive(Debug, Clone)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub arguments: String,
}

/// Accumulates per-index tool-call deltas (OpenAI streaming protocol).
#[derive(Default)]
pub struct ToolCallAssembler {
    slots: Vec<(Option<String>, Option<String>, String)>,
}

impl ToolCallAssembler {
    pub fn push(
        &mut self,
        index: usize,
        id: Option<String>,
        name: Option<String>,
        arguments_delta: String,
    ) {
        while self.slots.len() <= index {
            self.slots.push((None, None, String::new()));
        }
        let slot = &mut self.slots[index];
        if id.is_some() {
            slot.0 = id;
        }
        if name.is_some() {
            slot.1 = name;
        }
        slot.2.push_str(&arguments_delta);
    }

    pub fn finish(self) -> Vec<ToolCall> {
        self.slots
            .into_iter()
            .filter_map(|(id, name, arguments)| {
                Some(ToolCall {
                    id: id?,
                    name: name?,
                    arguments,
                })
            })
            .collect()
    }
}

/// Parse one SSE event (a `data: ...` line or multi-line block) into StreamEvent.
/// Returns None for keep-alives/comments; Err only for malformed JSON payloads.
pub fn parse_sse_data(payload: &str) -> Result<Option<StreamEvent>> {
    let payload = payload.trim();
    if payload.is_empty() || payload.starts_with(':') {
        return Ok(None);
    }
    if payload == "[DONE]" {
        return Ok(Some(StreamEvent::Done {
            finish: None,
            usage: None,
        }));
    }
    let v: Value = serde_json::from_str(payload).context("sse json")?;

    // usage may arrive on a chunk with empty choices or alongside final choice
    let usage = v.get("usage").filter(|u| !u.is_null()).map(parse_usage);

    let Some(choice) = v.pointer("/choices/0").filter(|c| !c.is_null()) else {
        // usage-only final chunk
        if usage.is_some() {
            return Ok(Some(StreamEvent::Done {
                finish: None,
                usage,
            }));
        }
        return Ok(None);
    };

    if let Some(finish) = choice.get("finish_reason").and_then(|f| f.as_str()) {
        if finish.is_empty() {
            // fall through to delta parsing (some servers send both)
        } else {
            return Ok(Some(StreamEvent::Done {
                finish: Some(finish.to_string()),
                usage,
            }));
        }
    }

    let Some(delta) = choice.get("delta") else {
        if usage.is_some() {
            return Ok(Some(StreamEvent::Done {
                finish: None,
                usage,
            }));
        }
        return Ok(None);
    };

    if let Some(rc) = delta.get("reasoning_content").and_then(|c| c.as_str())
        && !rc.is_empty()
    {
        return Ok(Some(StreamEvent::ReasoningDelta(rc.to_string())));
    }
    if let Some(content) = delta.get("content").and_then(|c| c.as_str())
        && !content.is_empty()
    {
        return Ok(Some(StreamEvent::TextDelta(content.to_string())));
    }
    if let Some(tcs) = delta.get("tool_calls").and_then(|t| t.as_array())
        && let Some(tc) = tcs.first()
    {
        let index = tc.get("index").and_then(|i| i.as_u64()).unwrap_or(0) as usize;
        return Ok(Some(StreamEvent::ToolCallDelta {
            index,
            id: tc.pointer("/id").and_then(|i| i.as_str()).map(String::from),
            name: tc
                .pointer("/function/name")
                .and_then(|n| n.as_str())
                .map(String::from),
            arguments_delta: tc
                .pointer("/function/arguments")
                .and_then(|a| a.as_str())
                .unwrap_or("")
                .to_string(),
        }));
    }
    Ok(None)
}

fn parse_usage(u: &Value) -> Usage {
    Usage {
        prompt_tokens: u.get("prompt_tokens").and_then(|v| v.as_u64()).unwrap_or(0),
        completion_tokens: u
            .get("completion_tokens")
            .and_then(|v| v.as_u64())
            .unwrap_or(0),
        total_tokens: u.get("total_tokens").and_then(|v| v.as_u64()).unwrap_or(0),
        cached_tokens: u
            .pointer("/prompt_tokens_details/cached_tokens")
            .and_then(|v| v.as_u64())
            .unwrap_or(0),
    }
}

/// Split a raw SSE byte stream into `data:` payloads (records separated by
/// blank lines). Pure so fixtures replay byte-for-byte in tests.
pub fn split_sse_frames(raw: &[u8]) -> Vec<String> {
    let text = String::from_utf8_lossy(raw);
    let mut out = Vec::new();
    for block in text.split("\n\n") {
        let block = block.trim_start_matches('\n');
        if block.is_empty() {
            continue;
        }
        let mut payload = String::new();
        for line in block.split('\n') {
            if let Some(rest) = line.strip_prefix("data:") {
                if !payload.is_empty() {
                    payload.push('\n');
                }
                payload.push_str(rest.trim_start());
            }
        }
        if !payload.is_empty() {
            out.push(payload);
        }
    }
    out
}

/// Replay a recorded stream fixture: parse every frame, collect events.
pub fn replay_fixture(raw: &[u8]) -> Result<Vec<StreamEvent>> {
    let mut events = Vec::new();
    for frame in split_sse_frames(raw) {
        if let Some(ev) = parse_sse_data(&frame)? {
            events.push(ev);
        }
    }
    Ok(events)
}

/// Build the OpenAI-compatible chat request body.
pub fn build_request(
    model: &str,
    messages: &[ChatMessage],
    max_tokens: Option<u32>,
    with_tools: bool,
) -> Value {
    let mut body = json!({
        "model": model,
        "messages": messages
            .iter()
            .map(|m| json!({"role": m.role, "content": m.content}))
            .collect::<Vec<_>>(),
        "stream": true,
        "stream_options": {"include_usage": true},
    });
    if let Some(mt) = max_tokens {
        body["max_tokens"] = json!(mt);
    }
    if with_tools {
        // tools are injected by the caller via `tools` — kept out of M2 body
        // until the tool loop lands (PLAN M2b)
        let _ = &body;
    }
    body
}

/// Live openai-compatible client (base_url + bearer key).
pub struct Client {
    http: reqwest::Client,
    base_url: String,
    api_key: String,
}

impl Client {
    pub fn new(base_url: impl Into<String>, api_key: impl Into<String>) -> Self {
        Self {
            http: reqwest::Client::new(),
            base_url: base_url.into().trim_end_matches('/').to_string(),
            api_key: api_key.into(),
        }
    }

    /// POST /chat/completions (stream) → boxed stream of parsed events.
    pub async fn chat_stream(
        &self,
        model: &str,
        messages: &[ChatMessage],
        max_tokens: Option<u32>,
    ) -> Result<futures_util::stream::BoxStream<'static, Result<StreamEvent>>> {
        let body = build_request(model, messages, max_tokens, false);
        let resp = self
            .http
            .post(format!("{}/chat/completions", self.base_url))
            .bearer_auth(&self.api_key)
            .header("content-type", "application/json")
            .header("accept", "text/event-stream")
            .body(serde_json::to_vec(&body)?)
            .send()
            .await
            .context("provider request")?;
        let status = resp.status();
        if !status.is_success() {
            let text = resp.text().await.unwrap_or_default();
            anyhow::bail!("provider {status}: {}", &text[..text.len().min(400)]);
        }
        let src = resp.bytes_stream();
        // Buffered line parser: a `data:` line can span TCP chunks (live 500s
        // proved it — M2c). Buffer until complete lines; keep remainder.
        let parsed = futures_util::stream::unfold(
            (src, Vec::<u8>::new()),
            |(mut src, mut buf)| async move {
                loop {
                    // complete line available?
                    if let Some(pos) = buf.iter().position(|b| *b == b'\n') {
                        let line: Vec<u8> = buf.drain(..=pos).collect();
                        let line = String::from_utf8_lossy(&line);
                        let line = line.trim_end_matches('\r').trim_end_matches('\n');
                        if let Some(payload) = line.strip_prefix("data:") {
                            match parse_sse_data(payload.trim()) {
                                Ok(Some(ev)) => return Some((Ok(ev), (src, buf))),
                                Ok(None) => continue, // keep-alive/blank → next line
                                Err(e) => return Some((Err(e), (src, buf))),
                            }
                        }
                        continue; // non-data line (blank separator etc.)
                    }
                    // need more bytes
                    match src.next().await {
                        Some(Ok(chunk)) => buf.extend_from_slice(&chunk),
                        Some(Err(e)) => {
                            return Some((
                                Err(anyhow::anyhow!("provider stream: {e}")),
                                (src, buf),
                            ));
                        }
                        None => {
                            // EOF: parse a trailing line without newline if present
                            if !buf.is_empty() {
                                let rest = String::from_utf8_lossy(&buf).trim_end().to_string();
                                buf.clear();
                                if let Some(payload) = rest.strip_prefix("data:") {
                                    match parse_sse_data(payload.trim()) {
                                        Ok(Some(ev)) => return Some((Ok(ev), (src, buf))),
                                        Ok(None) => {}
                                        Err(e) => return Some((Err(e), (src, buf))),
                                    }
                                }
                            }
                            return None;
                        }
                    }
                }
            },
        );
        Ok(Box::pin(parsed))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const STOP_FIXTURE: &[u8] = include_bytes!("../../../testdata/m2/llm_stream_stop.bin");
    const REASONING_FIXTURE: &[u8] = include_bytes!("../../../testdata/m2/llm_stream_deepseek.bin");

    /// K-PROVIDER part 1: recorded stream parses; text assembles byte-exact.
    #[test]
    fn fixture_stop_assembles_exact_text() {
        let events = replay_fixture(STOP_FIXTURE).expect("fixture parses");
        let mut text = String::new();
        let mut finish = None;
        let mut usage = None;
        for ev in &events {
            match ev {
                StreamEvent::TextDelta(t) => text.push_str(t),
                StreamEvent::Done {
                    finish: f,
                    usage: u,
                } => {
                    if f.is_some() {
                        finish = f.clone();
                    }
                    if u.is_some() {
                        usage = u.clone();
                    }
                }
                StreamEvent::ReasoningDelta(_) | StreamEvent::ToolCallDelta { .. } => {}
            }
        }
        assert_eq!(text, "HELLO_REFINE", "text assembly drifted from fixture");
        assert_eq!(finish.as_deref(), Some("stop"));
        let u = usage.expect("usage captured");
        assert_eq!(u.completion_tokens, 34);
        assert_eq!(u.total_tokens, u.prompt_tokens + u.completion_tokens);
    }

    /// K-PROVIDER part 2: reasoning deltas + finish=length + usage-only chunk.
    #[test]
    fn fixture_reasoning_fields() {
        let events = replay_fixture(REASONING_FIXTURE).expect("fixture parses");
        let has_reasoning = events
            .iter()
            .any(|e| matches!(e, StreamEvent::ReasoningDelta(r) if !r.is_empty()));
        assert!(has_reasoning, "reasoning deltas present in fixture");
        let finish = events.iter().find_map(|e| match e {
            StreamEvent::Done {
                finish: Some(f), ..
            } => Some(f.clone()),
            _ => None,
        });
        assert_eq!(finish.as_deref(), Some("length"));
        // content still accumulates (truncated)
        let text: String = events
            .iter()
            .filter_map(|e| match e {
                StreamEvent::TextDelta(t) => Some(t.as_str()),
                _ => None,
            })
            .collect();
        assert!(!text.is_empty());
    }

    /// Tool-call delta assembly (OpenAI protocol) — crafted per spec; fuzz
    /// corpus replaces this once cargo-fuzz lands (TESTING §6).
    #[test]
    fn tool_call_assembler_merges_deltas() {
        let mut a = ToolCallAssembler::default();
        a.push(0, Some("call_1".into()), Some("bash".into()), "".into());
        a.push(0, None, None, "{\"command\":".into());
        a.push(0, None, None, "\"ls\"}".into());
        a.push(1, Some("call_2".into()), Some("read".into()), "".into());
        a.push(1, None, None, "{\"path\":\"x\"}".into());
        let calls = a.finish();
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].name, "bash");
        assert_eq!(calls[0].arguments, "{\"command\":\"ls\"}");
        assert_eq!(calls[1].name, "read");
        assert_eq!(calls[1].arguments, "{\"path\":\"x\"}");
    }

    /// parse_sse_data edge cases: keep-alive, empty, malformed.
    #[test]
    fn sse_edge_cases() {
        assert!(parse_sse_data("").unwrap().is_none());
        assert!(parse_sse_data(": ping").unwrap().is_none());
        assert!(parse_sse_data("{not json").is_err());
        assert!(matches!(
            parse_sse_data("[DONE]").unwrap(),
            Some(StreamEvent::Done { .. })
        ));
        // usage-only chunk (empty choices)
        let ev = parse_sse_data(
            r#"{"choices":[],"usage":{"prompt_tokens":1,"completion_tokens":2,"total_tokens":3}}"#,
        )
        .unwrap();
        assert!(matches!(ev, Some(StreamEvent::Done { usage: Some(_), .. })));
    }
}
