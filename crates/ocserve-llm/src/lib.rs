//! ocserve-llm: provider clients. M2 scope: openai-compatible streaming with
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
    /// OpenAI tool-result linkage (role="tool").
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
    /// Assistant tool-call declarations (role="assistant", finish=tool_calls).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<Vec<Value>>,
}

impl ChatMessage {
    pub fn text(role: &str, content: impl Into<String>) -> Self {
        Self {
            role: role.to_string(),
            content: content.into(),
            tool_call_id: None,
            tool_calls: None,
        }
    }

    pub fn tool_result(tool_call_id: impl Into<String>, content: impl Into<String>) -> Self {
        Self {
            role: "tool".to_string(),
            content: content.into(),
            tool_call_id: Some(tool_call_id.into()),
            tool_calls: None,
        }
    }

    pub fn assistant_with_tools(content: impl Into<String>, tool_calls: Vec<Value>) -> Self {
        Self {
            role: "assistant".to_string(),
            content: content.into(),
            tool_call_id: None,
            tool_calls: Some(tool_calls),
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Usage {
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    pub total_tokens: u64,
    pub cached_tokens: u64,
    /// Provider-reported reasoning tokens (OpenAI-compatible
    /// `completion_tokens_details.reasoning_tokens`). Upstream Session.getUsage
    /// splits these out of `output` so the context meter counts them once.
    pub reasoning_tokens: u64,
}

/// Provider stream events (parsed from OpenAI-compatible SSE `data:` lines).
#[derive(Debug, Clone, PartialEq)]
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
        reasoning_tokens: u
            .pointer("/completion_tokens_details/reasoning_tokens")
            .or_else(|| u.get("reasoning_tokens"))
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

/// Incremental SSE line parser — the LIVE chat_stream path (M2c: a `data:`
/// line can span TCP chunks). Extracted verbatim from the stream unfold so
/// chunk-boundary splits are fuzzable: the seeded split test proves any
/// TCP chunking yields the identical event sequence (SRE nightly claim).
#[derive(Default)]
pub struct SseLineParser {
    buf: Vec<u8>,
    pending: std::collections::VecDeque<anyhow::Result<StreamEvent>>,
    eof: bool,
}

impl SseLineParser {
    pub fn new() -> Self {
        Self::default()
    }

    /// Feed one network chunk; completed lines parse immediately, in order.
    pub fn push(&mut self, chunk: &[u8]) {
        self.buf.extend_from_slice(chunk);
        while let Some(pos) = self.buf.iter().position(|b| *b == b'\n') {
            let line: Vec<u8> = self.buf.drain(..=pos).collect();
            let line = String::from_utf8_lossy(&line);
            let line = line.trim_end_matches('\r').trim_end_matches('\n');
            if let Some(payload) = line.strip_prefix("data:") {
                match parse_sse_data(payload.trim()) {
                    Ok(Some(ev)) => self.pending.push_back(Ok(ev)),
                    Ok(None) => continue, // keep-alive/blank → skip
                    Err(e) => self.pending.push_back(Err(e)),
                }
            }
            // non-data line: skip
        }
    }

    /// EOF: a trailing line without newline parses once (original semantics).
    pub fn finish(&mut self) {
        self.eof = true;
        if !self.buf.is_empty() {
            let rest = String::from_utf8_lossy(&self.buf).trim_end().to_string();
            self.buf.clear();
            if let Some(payload) = rest.strip_prefix("data:") {
                match parse_sse_data(payload.trim()) {
                    Ok(Some(ev)) => self.pending.push_back(Ok(ev)),
                    Ok(None) => {}
                    Err(e) => self.pending.push_back(Err(e)),
                }
            }
        }
    }

    /// Next completed event in stream order.
    pub fn pop(&mut self) -> Option<anyhow::Result<StreamEvent>> {
        self.pending.pop_front()
    }

    /// True when EOF seen and nothing pending — the stream ends here.
    pub fn is_done(&self) -> bool {
        self.eof && self.pending.is_empty()
    }
}

/// Context-overflow classifier (M6, COMPACTION D3): provider rejected the
/// REQUEST as too big (upstream processor.ts:629 ContextOverflowError
/// class). False positives compact a healthy session; false negatives
/// leave users stuck on 400s — both directions carry tests.
pub fn looks_like_context_overflow(err_msg: &str) -> bool {
    let m = err_msg.to_ascii_lowercase();
    // must look like a request-size rejection, not auth/rate-limit/5xx
    let status_ok = m.contains("provider 400")
        || m.contains("provider 413")
        || m.contains(" status 400")
        || m.contains(" status 413");
    if !status_ok {
        return false;
    }
    const MARKERS: &[&str] = &[
        "context length",
        "context window",
        "context_length",
        "context_length_exceeded",
        "maximum context",
        "too many tokens",
        "too long",
        "input length",
        "reduce the length",
        "prompt is too long",
        "exceeds the model",
        "exceeds max",
        "maximum context length",
        "string too long",
        "input is too long",
        "tokens exceeds",
        "exceeded the maximum",
        "request too large",
        "payload too large",
        "max context",
    ];
    MARKERS.iter().any(|k| m.contains(k))
}

/// Request shaping from the `chat.params` / `chat.headers` plugin hooks
/// (v1 parity: session/llm/request.ts:115/135). `Default` reproduces the
/// pre-hook behavior byte-for-byte (no temperature, no extra headers).
#[derive(Clone, Debug, Default)]
pub struct ChatOpts {
    pub temperature: Option<f64>,
    pub top_p: Option<f64>,
    pub top_k: Option<u32>,
    pub max_tokens: Option<u32>,
    /// `options` object from chat.params — merged into the body top-level
    /// (v1 providerOptions semantics; OpenAI-compatible endpoints ignore
    /// unknown keys). Explicit fields above override the same keys.
    pub options: Value,
    /// Extra headers from chat.headers. Invalid names/values are skipped
    /// with a warning — a misbehaving plugin can never break the request.
    pub headers: Vec<(String, String)>,
    /// Explicit `tool_choice` for requests that carry tools (default
    /// "auto"). Zen text-only calls (compaction, tools-off rounds) set
    /// "none": the free-tier gate requires a tools array (probe P8) but
    /// summarization must never emit tool calls (bench/zen-probe FINDINGS).
    pub tool_choice: Option<String>,
}

/// Build the OpenAI-compatible chat request body.
pub fn build_request(
    model: &str,
    messages: &[ChatMessage],
    opts: &ChatOpts,
    tools: Option<&[Value]>,
) -> Value {
    let mut body = json!({
        "model": model,
        "messages": serde_json::to_value(messages).expect("messages serialize"),
        "stream": true,
        "stream_options": {"include_usage": true},
    });
    // options first, explicit params override (precedence matches the
    // upstream mergeOptions chain feeding providerOptions + params).
    if let Some(obj) = opts.options.as_object() {
        for (k, v) in obj {
            body[k.clone()] = v.clone();
        }
    }
    if let Some(t) = opts.temperature {
        body["temperature"] = json!(t);
    }
    if let Some(t) = opts.top_p {
        body["top_p"] = json!(t);
    }
    if let Some(k) = opts.top_k {
        body["top_k"] = json!(k);
    }
    if let Some(mt) = opts.max_tokens {
        body["max_tokens"] = json!(mt);
    }
    if let Some(t) = tools
        && !t.is_empty()
    {
        body["tools"] = serde_json::to_value(t).expect("tools serialize");
        body["tool_choice"] = match &opts.tool_choice {
            Some(tc) => Value::String(tc.clone()),
            None => json!("auto"),
        };
    }
    body
}

/// Live openai-compatible client (base_url + bearer key).
pub struct Client {
    http: reqwest::Client,
    base_url: String,
    api_key: String,
}

/// Free-tier zen gate (bench/zen-probe/FINDINGS: probes P5 vs P7 — the
/// composite UA is load-bearing; the client/project/request headers are not).
/// Sent ONLY on `opencode.ai` bases; other providers keep reqwest's default.
pub const ZEN_FREE_TIER_UA: &str =
    "opencode/1.18.31 ai-sdk/provider-utils/4.0.23 runtime/bun/1.3.14";

impl Client {
    pub fn new(base_url: impl Into<String>, api_key: impl Into<String>) -> Self {
        Self {
            http: reqwest::Client::new(),
            base_url: base_url.into().trim_end_matches('/').to_string(),
            api_key: api_key.into(),
        }
    }

    /// opencode.ai endpoint (zen free tier or opencode-go gateway).
    /// The free-tier gate requires the composite UA on every request and a
    /// `tools` array in the body (FINDINGS: P5/P6/P7/P8) — callers shape
    /// text-only requests (compaction / tools-off rounds) with this.
    pub fn is_zen(&self) -> bool {
        self.base_url.contains("opencode.ai")
    }

    /// POST /chat/completions (stream) → boxed stream of parsed events.
    pub async fn chat_stream(
        &self,
        model: &str,
        messages: &[ChatMessage],
        opts: &ChatOpts,
        tools: Option<&[Value]>,
        session_id: Option<&str>,
    ) -> Result<futures_util::stream::BoxStream<'static, Result<StreamEvent>>> {
        let body = build_request(model, messages, opts, tools);
        let mut req = self
            .http
            .post(format!("{}/chat/completions", self.base_url))
            .bearer_auth(&self.api_key)
            .header("content-type", "application/json")
            .header("accept", "text/event-stream");
        // plugin-injected headers (chat.headers) — validated, never fatal
        for (k, v) in &opts.headers {
            match reqwest::header::HeaderName::from_bytes(k.as_bytes()) {
                Ok(name) => match reqwest::header::HeaderValue::from_str(v) {
                    Ok(val) => req = req.header(name, val),
                    Err(_) => tracing::warn!("chat.headers: invalid value for header {k:?}"),
                },
                Err(_) => tracing::warn!("chat.headers: invalid header name {k:?}"),
            }
        }
        // zen free-tier gate: composite UA is load-bearing (P5 passes with
        // it, P7 fails without — FINDINGS). Plugin-set UA wins if present.
        if self.is_zen()
            && !opts
                .headers
                .iter()
                .any(|(k, _)| k.eq_ignore_ascii_case("user-agent"))
        {
            req = req.header("user-agent", ZEN_FREE_TIER_UA);
        }
        // opencode-go gateway requires it for routing (live 400:
        // MissingSessionID — user's oc-remote send hit this 2026-10-02);
        // direct providers ignore unknown headers.
        if let Some(sid) = session_id {
            req = req.header("x-opencode-session", sid);
        }
        // K-EFFICIENCY (bytehound phase 1): pre-size the serialization
        // buffer — serde_json::to_vec grows by doubling (≈log2(N) reallocs
        // + memcpy of the whole multi-MB history, every round, every
        // concurrent prompt). One exact alloc instead.
        let est = estimate_request_bytes(messages, tools);
        let mut buf = Vec::with_capacity(est);
        serde_json::to_writer(&mut buf, &body)?;
        let resp = req.body(buf).send().await.context("provider request")?;
        let status = resp.status();
        if !status.is_success() {
            let text = resp.text().await.unwrap_or_default();
            // Gate-change visibility (K-MODEL-STATE): opencode can flip the
            // free-tier wall anytime — count it, the prompt error still
            // surfaces via session.error.
            if status == reqwest::StatusCode::FORBIDDEN
                && self.is_zen()
                && text.contains("FreeTierError")
            {
                ocserve_metrics::labeled_counter(
                    "ocserve_zen_freetier_total",
                    "result=\"gate\"",
                    1,
                );
            }
            anyhow::bail!("provider {status}: {}", &text[..text.len().min(400)]);
        }
        let src = resp.bytes_stream();
        // Buffered line parser: a `data:` line can span TCP chunks (live 500s
        // proved it — M2c). Buffer until complete lines; keep remainder.
        // SseLineParser = the exact prior inline loop (M2c line buffering),
        // extracted so the chunk-split fuzz exercises production code.
        let parsed = futures_util::stream::unfold(
            (src, SseLineParser::new()),
            |(mut src, mut parser)| async move {
                loop {
                    if let Some(ev) = parser.pop() {
                        return Some((ev, (src, parser)));
                    }
                    if parser.is_done() {
                        return None;
                    }
                    match src.next().await {
                        Some(Ok(chunk)) => parser.push(&chunk),
                        Some(Err(e)) => {
                            return Some((
                                Err(anyhow::anyhow!("provider stream: {e}")),
                                (src, parser),
                            ));
                        }
                        None => parser.finish(),
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

    /// x-opencode-session must ride on provider requests when a session is
    /// known (opencode-go gateway 400s MissingSessionID without it — the
    /// user's oc-remote send hit exactly this on 2026-10-02; live-verified
    /// HDR_OK after the fix, this test pins the wire bytes).
    #[tokio::test]
    async fn chat_stream_sends_session_header_when_provided() {
        use tokio::io::AsyncReadExt as _;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (tx, rx) = tokio::sync::oneshot::channel::<String>();
        tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            let mut got = String::new();
            let mut buf = [0u8; 4096];
            loop {
                let n = sock.read(&mut buf).await.unwrap_or(0);
                got.push_str(&String::from_utf8_lossy(&buf[..n]));
                if got.contains("\r\n\r\n") || n == 0 {
                    break;
                }
            }
            let _ = tx.send(got);
            // close without a response: chat_stream errors AFTER the request
            // bytes (with headers) are on the wire — that's the assertion point
        });
        let client = Client::new(format!("http://{addr}"), "key");
        let msgs = vec![ChatMessage::text("user", "hi")];
        let _ = client
            .chat_stream("m", &msgs, &ChatOpts::default(), None, Some("ses_hdr_test"))
            .await;
        let req = tokio::time::timeout(std::time::Duration::from_secs(5), rx)
            .await
            .expect("captured request")
            .unwrap();
        let lower = req.to_ascii_lowercase();
        assert!(
            lower.contains("x-opencode-session: ses_hdr_test"),
            "session header missing from request head:\n{req}"
        );
        // Authorization still present alongside it
        assert!(lower.contains("authorization: bearer"), "auth header gone");
    }

    /// chat.params → body mapping (v1 llm/request.ts:115 shape: topP/topK/
    /// maxOutputTokens camelCase in, snake_case on the wire), options merge,
    /// and explicit-field precedence over same-key options.
    #[test]
    fn build_request_applies_chat_params() {
        let msgs = vec![ChatMessage::text("user", "hi")];
        let opts = ChatOpts {
            temperature: Some(0.7),
            top_p: Some(0.9),
            top_k: Some(40),
            max_tokens: Some(123),
            options: json!({"temperature": 0.1, "x_hook": true}),
            headers: vec![],
            tool_choice: None,
        };
        let body = build_request("m", &msgs, &opts, None);
        assert_eq!(body["temperature"], json!(0.7), "explicit beats options");
        assert_eq!(body["top_p"], json!(0.9));
        assert_eq!(body["top_k"], json!(40));
        assert_eq!(body["max_tokens"], json!(123));
        assert_eq!(body["x_hook"], json!(true), "options merged");
        // default opts → body has none of the hook fields (pre-hook parity)
        let plain = build_request("m", &msgs, &ChatOpts::default(), None);
        for k in ["temperature", "top_p", "top_k", "max_tokens"] {
            assert!(plain.get(k).is_none(), "{k} must be absent by default");
        }
    }

    /// chat.headers → wire (positive) + invalid names/values skipped without
    /// failing the request (a plugin can never break the call).
    #[tokio::test]
    async fn chat_stream_applies_plugin_headers_and_skips_invalid() {
        use tokio::io::AsyncReadExt as _;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (tx, rx) = tokio::sync::oneshot::channel::<String>();
        tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            let mut got = String::new();
            let mut buf = [0u8; 8192];
            loop {
                let n = sock.read(&mut buf).await.unwrap_or(0);
                got.push_str(&String::from_utf8_lossy(&buf[..n]));
                if got.contains("\r\n\r\n") || n == 0 {
                    break;
                }
            }
            let _ = tx.send(got);
        });
        let client = Client::new(format!("http://{addr}"), "key");
        let msgs = vec![ChatMessage::text("user", "hi")];
        let opts = ChatOpts {
            temperature: Some(0.7),
            headers: vec![
                ("x-plugin".into(), "yes".into()),
                ("bad\nname".into(), "skipped".into()),
                ("x-bad-value".into(), "v\x7f".into()),
            ],
            ..ChatOpts::default()
        };
        let _ = client.chat_stream("m", &msgs, &opts, None, None).await;
        let req = tokio::time::timeout(std::time::Duration::from_secs(5), rx)
            .await
            .expect("captured request")
            .unwrap();
        assert!(
            req.to_ascii_lowercase().contains("x-plugin: yes"),
            "plugin header missing:\n{req}"
        );
        assert!(
            !req.to_ascii_lowercase().contains("bad\nname"),
            "invalid header name must be skipped, not sent"
        );
        assert!(req.contains("0.7"), "temperature must reach the body");
    }

    /// Absent session → header absent (direct providers see no change).
    #[tokio::test]
    async fn chat_stream_omits_session_header_when_none() {
        use tokio::io::AsyncReadExt as _;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (tx, rx) = tokio::sync::oneshot::channel::<String>();
        tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            let mut got = String::new();
            let mut buf = [0u8; 4096];
            loop {
                let n = sock.read(&mut buf).await.unwrap_or(0);
                got.push_str(&String::from_utf8_lossy(&buf[..n]));
                if got.contains("\r\n\r\n") || n == 0 {
                    break;
                }
            }
            let _ = tx.send(got);
        });
        let client = Client::new(format!("http://{addr}"), "key");
        let msgs = vec![ChatMessage::text("user", "hi")];
        let _ = client
            .chat_stream("m", &msgs, &ChatOpts::default(), None, None)
            .await;
        let req = tokio::time::timeout(std::time::Duration::from_secs(5), rx)
            .await
            .expect("captured request")
            .unwrap();
        assert!(
            !req.to_ascii_lowercase().contains("x-opencode-session"),
            "header must be omitted when no session: {req}"
        );
    }

    const STOP_FIXTURE: &[u8] = include_bytes!("../testdata/llm_stream_stop.bin");
    const REASONING_FIXTURE: &[u8] = include_bytes!("../testdata/llm_stream_deepseek.bin");

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
        assert_eq!(text, "HELLO_OCSERVE", "text assembly drifted from fixture");
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

    /// K-PROVIDER stream fuzz (SRE nightly claim, now enforced): 10,000
    /// seeded chunk-boundary splits of each recorded fixture through the
    /// LIVE line parser must yield the identical event sequence as the
    /// whole-buffer parse — TCP chunking can never change results.
    #[test]
    fn chunk_split_fuzz_matches_whole_buffer_parse() {
        for fixture in [STOP_FIXTURE, REASONING_FIXTURE] {
            // whole-buffer reference through the SAME parser
            let mut whole = SseLineParser::new();
            whole.push(fixture);
            whole.finish();
            let expected: Vec<StreamEvent> =
                std::iter::from_fn(|| whole.pop().map(|ev| ev.expect("whole parse"))).collect();
            assert!(!expected.is_empty(), "fixture produced no events");
            let is_stop = std::ptr::eq(fixture.as_ptr(), STOP_FIXTURE.as_ptr());

            let mut seed: u32 = 0x9E37_79B9;
            let mut stop_text = String::new();
            for _ in 0..10_000 {
                // xorshift32 + LCG — deterministic, no rng dependency
                seed ^= seed << 13;
                seed ^= seed >> 17;
                seed ^= seed << 5;
                let mut parser = SseLineParser::new();
                let mut off = 0usize;
                let mut s = seed as u64;
                while off < fixture.len() {
                    s = s.wrapping_mul(1664525).wrapping_add(1013904223);
                    let step = 1 + (s as usize % 512);
                    let end = (off + step).min(fixture.len());
                    parser.push(&fixture[off..end]);
                    off = end;
                }
                parser.finish();
                let mut got: Vec<StreamEvent> = Vec::new();
                let mut text = String::new();
                while let Some(ev) = parser.pop() {
                    let ev =
                        ev.unwrap_or_else(|e| panic!("chunked parse error at seed {seed}: {e}"));
                    if let StreamEvent::TextDelta(t) = &ev {
                        text.push_str(t);
                    }
                    got.push(ev);
                }
                assert_eq!(
                    got,
                    expected,
                    "chunk-split divergence at seed {seed} (fixture {}B)",
                    fixture.len()
                );
                if is_stop {
                    stop_text = text;
                }
            }
            if is_stop {
                assert_eq!(stop_text, "HELLO_OCSERVE", "assembled text drifted");
            }
        }
    }
}

#[cfg(test)]
mod overflow_classifier_tests {
    use super::looks_like_context_overflow;

    #[test]
    fn detects_request_size_rejections() {
        assert!(looks_like_context_overflow(
            "provider 400: {\"error\":{\"message\":\"This model's maximum context length is 128000 tokens\"}}"
        ));
        assert!(looks_like_context_overflow(
            "provider 413: prompt is too long: 250000 tokens > 200000 maximum"
        ));
        assert!(looks_like_context_overflow(
            "provider 400: input length of 300000 exceeds maximum context length of 200000"
        ));
        assert!(looks_like_context_overflow(
            "provider 400: The request exceeds the model's context limit"
        ));
    }

    #[test]
    fn never_flags_auth_rate_limit_or_server_errors() {
        assert!(!looks_like_context_overflow(
            "provider 401: invalid api key"
        ));
        assert!(!looks_like_context_overflow(
            "provider 429: rate limit exceeded, retry later"
        ));
        assert!(!looks_like_context_overflow(
            "provider 500: internal server error"
        ));
        assert!(!looks_like_context_overflow(
            "provider stream open stalled: no response headers"
        ));
        assert!(!looks_like_context_overflow("connection reset by peer"));
        //400 without size markers is NOT overflow (validation error etc.)
        assert!(!looks_like_context_overflow(
            "provider 400: model 'nope' not found"
        ));
    }
}

/// Conservative request-size estimate for buffer pre-sizing (K-EFFICIENCY).
/// Over-estimates are fine (one exact alloc); under-estimates cost at most
/// one extra doubling — content + tool args dominate either way.
pub(crate) fn estimate_request_bytes(messages: &[ChatMessage], tools: Option<&[Value]>) -> usize {
    let mut n = 4096;
    for m in messages {
        n += m.content.len() + m.role.len() + 64;
        if let Some(id) = &m.tool_call_id {
            n += id.len();
        }
        if let Some(tcs) = &m.tool_calls {
            for tc in tcs {
                n += 256;
                if let Some(args) = tc.pointer("/function/arguments").and_then(|v| v.as_str()) {
                    n += args.len();
                }
            }
        }
    }
    if let Some(t) = tools {
        n += t.len() * 1024; // schemas: rough floor, covered by the slack below
    }
    n + n / 4
}

#[cfg(test)]
mod estimate_tests {
    use super::*;

    /// The pre-size must never pathologically under-estimate (a bad floor
    /// would silently restore the doubling-growth we just removed).
    #[test]
    fn estimate_is_not_pathologically_small() {
        let msgs = vec![
            ChatMessage::text("user", "hello world"),
            ChatMessage::assistant_with_tools(
                "thinking",
                vec![serde_json::json!({
                    "id": "call_1",
                    "type": "function",
                    "function": {"name": "bash", "arguments": "x".repeat(40_000)},
                })],
            ),
            ChatMessage::tool_result("call_1", "y".repeat(80_000)),
        ];
        let est = estimate_request_bytes(&msgs, None);
        let actual = serde_json::to_vec(&crate::build_request(
            "m",
            &msgs,
            &ChatOpts::default(),
            None,
        ))
        .unwrap()
        .len();
        assert!(
            est >= actual,
            "estimate {est} < actual {actual} — would re-grow"
        );
    }

    /// Zen free-tier gate (FINDINGS P5 vs P7): the composite UA rides on
    /// opencode.ai bases ONLY — other providers keep reqwest's default.
    /// Path trick: base `/opencode.ai/...` flips the substring predicate
    /// while still connecting to the local mock.
    #[tokio::test]
    async fn chat_stream_sends_composite_ua_only_on_zen_bases() {
        use tokio::io::AsyncReadExt as _;
        async fn capture(path_suffix: &str) -> String {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            let (tx, rx) = tokio::sync::oneshot::channel::<String>();
            tokio::spawn(async move {
                let (mut sock, _) = listener.accept().await.unwrap();
                let mut got = String::new();
                let mut buf = [0u8; 4096];
                loop {
                    let n = sock.read(&mut buf).await.unwrap_or(0);
                    got.push_str(&String::from_utf8_lossy(&buf[..n]));
                    if got.contains("\r\n\r\n") || n == 0 {
                        break;
                    }
                }
                let _ = tx.send(got);
            });
            let client = Client::new(format!("http://{addr}{path_suffix}"), "public");
            assert_eq!(
                client.is_zen(),
                path_suffix.contains("opencode.ai"),
                "fixture must exercise the right predicate branch"
            );
            let msgs = vec![ChatMessage::text("user", "hi")];
            let _ = client
                .chat_stream(
                    "m",
                    &msgs,
                    &ChatOpts::default(),
                    None,
                    Some("ses_0123456789abcdef01234567"),
                )
                .await;
            tokio::time::timeout(std::time::Duration::from_secs(5), rx)
                .await
                .expect("captured request")
                .unwrap()
        }

        let zen = capture("/opencode.ai/zen/v1").await.to_ascii_lowercase();
        let ua = ZEN_FREE_TIER_UA.to_ascii_lowercase();
        assert!(
            zen.contains(&format!("user-agent: {ua}")),
            "zen base must carry the composite UA (P5/P7 — gate rejects plain UA): {zen}"
        );
        let other = capture("/v1").await.to_ascii_lowercase();
        assert!(
            !other.contains("opencode/1.18.31"),
            "non-zen base must NOT carry the composite UA: {other}"
        );
    }

    /// tool_choice: default "auto" with tools; explicit override honored
    /// (zen text-only calls pin "none" — FINDINGS P8); no tools → no key.
    #[test]
    fn build_request_tool_choice_defaults_auto_and_honors_override() {
        let msgs = vec![ChatMessage::text("user", "hi")];
        let tools = vec![json!({
            "type": "function",
            "function": {"name": "bash", "description": "d", "parameters": {"type": "object"}}
        })];
        let auto = build_request("m", &msgs, &ChatOpts::default(), Some(&tools));
        assert_eq!(auto["tool_choice"], json!("auto"));
        let opts = ChatOpts {
            tool_choice: Some("none".into()),
            ..Default::default()
        };
        let none = build_request("m", &msgs, &opts, Some(&tools));
        assert_eq!(none["tool_choice"], json!("none"), "override honored");
        assert!(none["tools"].is_array(), "tools still present for the gate");
        let no_tools = build_request("m", &msgs, &opts, None);
        assert!(
            no_tools.get("tool_choice").is_none(),
            "no tools → no tool_choice key"
        );
    }

    #[test]
    fn is_zen_only_on_opencode_bases() {
        assert!(Client::new("https://opencode.ai/zen/v1", "public").is_zen());
        assert!(Client::new("https://opencode.ai/zen/go/v1", "k").is_zen());
        assert!(!Client::new("https://api.deepseek.com/v1", "k").is_zen());
        assert!(!Client::new("http://127.0.0.1:9", "k").is_zen());
    }
}
