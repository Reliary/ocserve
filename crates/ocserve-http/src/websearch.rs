//! Websearch provider backends (gap closure, D-WEBSEARCH).
//!
//! Upstream fans POST /api/websearch out to provider-specific search APIs,
//! keyed by the ACTIVE stored credential for that integration (the credential
//! store is the keyring — integrationIDs match provider ids). Verified live:
//! - missing providerID → 400 websearch_provider_required
//! - unknown providerID → 400 websearch_provider_not_found
//! - known provider, no active key → `{results: []}` (observed exa behavior)
//! - backend failure → 503 ServiceUnavailableError "Web search request
//!   failed: <id>" (observed tavily behavior under failure)
//!
//! Direct backends implemented where the provider REST is a stable,
//! documented, key-in-body/header POST: Tavily + Exa. Firecrawl / Parallel /
//! TinyFish return the backend-failure envelope (D-WEBSEARCH-PARTIAL — same
//! envelope upstream emits when ITS backend fails, never fabricated
//! results). Async reqwest (never blocking on workers — F1 convoy rule),
//! 20 s timeout (upstream tool uses 25 s for exa).

use serde_json::Value;

/// The five providers upstream lists (verified bytes — id + name only).
pub const PROVIDERS: [(&str, &str); 5] = [
    ("exa", "Exa"),
    ("firecrawl", "Firecrawl"),
    ("parallel", "Parallel"),
    ("tavily", "Tavily"),
    ("tinyfish", "TinyFish"),
];

pub fn is_known(id: &str) -> bool {
    PROVIDERS.iter().any(|(p, _)| *p == id)
}

/// Direct search supported without new deps (stable documented REST).
pub(crate) fn direct_supported(id: &str) -> bool {
    id == "tavily" || id == "exa"
}

/// One normalized result (WebSearch.Result wire shape).
pub struct Hit {
    pub url: String,
    pub title: Option<String>,
    pub content: Option<String>,
    pub published_ms: Option<i64>,
}

impl Hit {
    pub fn to_wire(&self) -> Value {
        let mut o = serde_json::Map::new();
        o.insert("url".into(), self.url.clone().into());
        if let Some(t) = &self.title {
            o.insert("title".into(), t.clone().into());
        }
        if let Some(c) = &self.content {
            o.insert("content".into(), c.clone().into());
        }
        // `time` is required by the schema; unknown → 0 (honest unknown,
        // never a fabricated date).
        o.insert(
            "time".into(),
            serde_json::json!({"published": self.published_ms.unwrap_or(0)}),
        );
        Value::Object(o)
    }
}

/// Extract the API key from an active credential value ({type:key,key}).
/// OAuth/external values are not API keys → None (keyless path).
pub fn api_key_of(value_json: &str) -> Option<String> {
    let v: Value = serde_json::from_str(value_json).ok()?;
    if v.get("type").and_then(|t| t.as_str()) != Some("key") {
        return None;
    }
    v.get("key").and_then(|k| k.as_str()).map(str::to_string)
}

fn client() -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(20))
        .user_agent("ocserve/1.0")
        .build()
        .expect("websearch client builds")
}

/// Search via a direct backend. `base` overrides the API root (tests point
/// it at a local stub; production passes None). Errors are opaque (the
/// handler maps every failure to the backend-failure envelope — provider
/// error bodies never leak to clients).
pub async fn search(
    provider_id: &str,
    api_key: &str,
    query: &str,
    base: Option<&str>,
) -> anyhow::Result<Vec<Hit>> {
    match provider_id {
        "tavily" => {
            let root = base.unwrap_or("https://api.tavily.com");
            let resp: Value = client()
                .post(format!("{root}/search"))
                .json(&serde_json::json!({
                    "api_key": api_key,
                    "query": query,
                    "max_results": 5,
                    "include_answer": false,
                }))
                .send()
                .await
                .map_err(|e| anyhow::anyhow!("tavily request: {e:#}"))?
                .error_for_status()
                .map_err(|e| anyhow::anyhow!("tavily status: {e:#}"))?
                .json()
                .await
                .map_err(|e| anyhow::anyhow!("tavily body: {e:#}"))?;
            Ok(resp
                .get("results")
                .and_then(|r| r.as_array())
                .cloned()
                .unwrap_or_default()
                .iter()
                .filter_map(|r| {
                    Some(Hit {
                        url: r.get("url")?.as_str()?.to_string(),
                        title: r.get("title").and_then(|t| t.as_str()).map(str::to_string),
                        content: r
                            .get("content")
                            .and_then(|t| t.as_str())
                            .map(str::to_string),
                        published_ms: None,
                    })
                })
                .collect())
        }
        "exa" => {
            let root = base.unwrap_or("https://api.exa.ai");
            let resp: Value = client()
                .post(format!("{root}/search"))
                .header("x-api-key", api_key)
                .json(&serde_json::json!({"query": query, "numResults": 5}))
                .send()
                .await
                .map_err(|e| anyhow::anyhow!("exa request: {e:#}"))?
                .error_for_status()
                .map_err(|e| anyhow::anyhow!("exa status: {e:#}"))?
                .json()
                .await
                .map_err(|e| anyhow::anyhow!("exa body: {e:#}"))?;
            Ok(resp
                .get("results")
                .and_then(|r| r.as_array())
                .cloned()
                .unwrap_or_default()
                .iter()
                .filter_map(|r| {
                    Some(Hit {
                        url: r.get("url")?.as_str()?.to_string(),
                        title: r.get("title").and_then(|t| t.as_str()).map(str::to_string),
                        content: r.get("text").and_then(|t| t.as_str()).map(str::to_string),
                        published_ms: None,
                    })
                })
                .collect())
        }
        other => anyhow::bail!("no direct backend for {other}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn provider_list_is_the_verified_five() {
        let ids: Vec<_> = PROVIDERS.iter().map(|(id, _)| *id).collect();
        assert_eq!(ids, ["exa", "firecrawl", "parallel", "tavily", "tinyfish"]);
        assert!(is_known("tavily") && !is_known("nope"));
        assert!(direct_supported("tavily") && direct_supported("exa"));
        assert!(!direct_supported("firecrawl"));
        // CONTROL: list missing a provider → assert fails, red.
    }

    #[test]
    fn api_key_extraction_only_for_key_values() {
        assert_eq!(
            api_key_of(r#"{"type":"key","key":"k123"}"#).as_deref(),
            Some("k123")
        );
        assert_eq!(api_key_of(r#"{"type":"oauth","methodID":"m"}"#), None);
        assert_eq!(api_key_of("not-json"), None);
        // CONTROL: extraction returning oauth values → assert fails, red.
    }

    #[test]
    fn hit_wire_shape_has_required_keys() {
        let h = Hit {
            url: "https://x".into(),
            title: Some("t".into()),
            content: None,
            published_ms: None,
        };
        let w = h.to_wire();
        assert_eq!(w["url"], "https://x");
        assert_eq!(w["time"]["published"], 0);
        assert!(w.get("content").is_none(), "absent, never null");
        // CONTROL: null content → assert fails, red.
    }

    /// Serve a canned Tavily-shaped body on loopback; search() must map it
    /// to Hits (url required, title/content optional). Proves the direct
    /// backend mapping without network.
    #[tokio::test]
    async fn tavily_direct_maps_results() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            use tokio::io::{AsyncBufReadExt, AsyncWriteExt};
            let mut reader = tokio::io::BufReader::new(&mut sock);
            let mut len = 0usize;
            loop {
                let mut line = String::new();
                reader.read_line(&mut line).await.unwrap();
                let line = line.trim_end().to_string();
                if line.is_empty() {
                    break;
                }
                if let Some(v) = line.strip_prefix("content-length:") {
                    len = v.trim().parse().unwrap_or(0);
                }
            }
            let mut body = vec![0u8; len];
            use tokio::io::AsyncReadExt;
            reader.read_exact(&mut body).await.unwrap();
            let payload = r#"{"results":[{"url":"https://example.com/a","title":"A","content":"body-a"},{"url":"https://example.com/b"}]}"#;
            let resp = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                payload.len(),
                payload
            );
            sock.write_all(resp.as_bytes()).await.unwrap();
        });
        let hits = search("tavily", "k", "q", Some(&format!("http://{addr}")))
            .await
            .expect("stub search works");
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].url, "https://example.com/a");
        assert_eq!(hits[0].title.as_deref(), Some("A"));
        assert_eq!(hits[0].content.as_deref(), Some("body-a"));
        assert_eq!(hits[1].url, "https://example.com/b");
        assert!(hits[1].title.is_none());
        // CONTROL: mapping that drops url-less rows silently → len wrong, red.
    }

    #[tokio::test]
    async fn unknown_provider_is_an_error_not_empty() {
        let r = search("nope", "k", "q", None).await;
        assert!(
            r.is_err(),
            "unknown provider must error (handler maps to 400/503, never empty)"
        );
    }
}
