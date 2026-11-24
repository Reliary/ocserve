//! OpenCode v2 `websearch` tool.
//!
//! Upstream fans the query out to the active WebSearch integration and returns
//! a Markdown digest. ocserve supports the two backends whose REST API is
//! stable and key-based (Tavily, Exa — the same pair its `/api/websearch`
//! route implements), reading the key from the environment first and then from
//! the legacy `auth.json` credential file the server already reads.
//!
//! Without a key the tool fails with an actionable message, matching upstream's
//! "select a provider" state. Named divergence `D-V2-WEBSEARCH-KEY`: the
//! credential may come from `TAVILY_API_KEY`/`EXA_API_KEY` in addition to the
//! stored credential, so a headless deployment can enable it without the UI.

use anyhow::{Context, Result, anyhow};
use serde_json::{Value, json};
use std::path::PathBuf;

use crate::ToolResult;

fn env_key(name: &str) -> Option<String> {
    std::env::var(name)
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
}

/// `~/.local/share/opencode/auth.json` (or `$XDG_DATA_HOME/opencode/…`),
/// `{provider: {type:"api", key}}`.
fn stored_key(provider: &str) -> Option<String> {
    let home = std::env::var("HOME").ok()?;
    let data = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(&home).join(".local/share"));
    let raw = std::fs::read_to_string(data.join("opencode/auth.json")).ok()?;
    let v: Value = serde_json::from_str(&raw).ok()?;
    v.get(provider)?
        .get("key")
        .and_then(Value::as_str)
        .map(str::to_string)
        .filter(|k| !k.is_empty())
}

fn key_for(provider: &str) -> Option<String> {
    match provider {
        "tavily" => env_key("TAVILY_API_KEY").or_else(|| stored_key("tavily")),
        "exa" => env_key("EXA_API_KEY").or_else(|| stored_key("exa")),
        _ => None,
    }
}

fn tavily(query: &str, key: &str) -> Result<Vec<Value>> {
    let client = reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(25))
        .user_agent("ocserve/1.0")
        .build()
        .context("websearch client")?;
    let resp = client
        .post("https://api.tavily.com/search")
        .json(&json!({"api_key": key, "query": query, "max_results": 5}))
        .send()
        .context("tavily request")?;
    let status = resp.status();
    if !status.is_success() {
        return Err(anyhow!(status_message(status.as_u16())));
    }
    let body: Value = resp.json().context("tavily body")?;
    Ok(body
        .get("results")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .map(|r| {
                    json!({
                        "url": r.get("url").cloned().unwrap_or(json!("")),
                        "title": r.get("title").cloned().unwrap_or(Value::Null),
                        "content": r.get("content").cloned().unwrap_or(Value::Null),
                        "time": {"published": 0},
                    })
                })
                .collect()
        })
        .unwrap_or_default())
}

fn exa(query: &str, key: &str) -> Result<Vec<Value>> {
    let client = reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(25))
        .user_agent("ocserve/1.0")
        .build()
        .context("websearch client")?;
    let resp = client
        .post("https://api.exa.ai/search")
        .header("x-api-key", key)
        .json(&json!({"query": query, "numResults": 5, "contents": {"text": true}}))
        .send()
        .context("exa request")?;
    let status = resp.status();
    if !status.is_success() {
        return Err(anyhow!(status_message(status.as_u16())));
    }
    let body: Value = resp.json().context("exa body")?;
    Ok(body
        .get("results")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .map(|r| {
                    json!({
                        "url": r.get("url").cloned().unwrap_or(json!("")),
                        "title": r.get("title").cloned().unwrap_or(Value::Null),
                        "content": r.get("text").cloned().or_else(|| r.get("summary").cloned()),
                        "time": {"published": 0},
                    })
                })
                .collect()
        })
        .unwrap_or_default())
}

fn status_message(status: u16) -> String {
    match status {
        429 => "Web search rate limited (HTTP 429)".into(),
        401 => "Web search authentication failed (HTTP 401)".into(),
        other => format!("Web search request failed (HTTP {other})"),
    }
}

/// Whether a backend key is available (env or legacy auth file). Used to stop
/// advertising a tool that can only fail.
pub fn configured() -> bool {
    key_for("tavily").is_some() || key_for("exa").is_some()
}

/// Execute the v2 `websearch` tool (`{query}`).
pub fn run(input: &Value) -> Result<ToolResult> {
    let query = input
        .get("query")
        .and_then(Value::as_str)
        .context("websearch.query is required")?;
    let (provider, results) = if let Some(key) = key_for("tavily") {
        ("tavily", tavily(query, &key)?)
    } else if let Some(key) = key_for("exa") {
        ("exa", exa(query, &key)?)
    } else {
        return Err(anyhow!(
            "Web search is not configured. Set TAVILY_API_KEY or EXA_API_KEY \
             (or store a Tavily/Exa API key) and try again."
        ));
    };
    let content = if results.is_empty() {
        "No search results found. Please try a different query.".to_string()
    } else {
        results
            .iter()
            .map(|r| {
                let title = r
                    .get("title")
                    .and_then(Value::as_str)
                    .or_else(|| r.get("url").and_then(Value::as_str))
                    .unwrap_or("result");
                let url = r.get("url").and_then(Value::as_str).unwrap_or("");
                let body = r.get("content").and_then(Value::as_str).unwrap_or("");
                if body.is_empty() {
                    format!("## [{title}]({url})")
                } else {
                    format!("## [{title}]({url})\n\n{body}")
                }
            })
            .collect::<Vec<_>>()
            .join("\n\n")
    };
    Ok(ToolResult {
        output: content,
        truncated: false,
        exit: None,
        title: query.to_string(),
        error: false,
        metadata: Some(json!({"provider": provider, "results": results})),
        output_path: None,
    })
}
