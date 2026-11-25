//! Bounded JSONC reader for OpenCode config documents.
//!
//! OpenCode v2 accepts `//` and `/* */` comments plus trailing commas in
//! `opencode.json`/`opencode.jsonc` (`ConfigDiscovery` + the JSONC parser in
//! `packages/core/src/config.ts`). `serde_json` accepts neither, so a stock v2
//! config would be reported as corrupt and the server would boot with
//! defaults. This is the one parser both the CLI loader and the HTTP config
//! routes use.

use anyhow::Result;
use serde_json::Value;

/// MEMORY §6 parse cap: a larger document is refused, never materialized.
pub const MAX_CONFIG_BYTES: usize = 8 * 1024 * 1024;

/// Strip JSONC comments and trailing commas, leaving a JSON document.
///
/// String literals are copied verbatim (escapes honoured), so `//` inside a
/// URL or string survives. Newlines inside block comments are kept so a parse
/// error still points at the right line.
pub fn strip(text: &str) -> Result<String> {
    anyhow::ensure!(
        text.len() <= MAX_CONFIG_BYTES,
        "config document is {} bytes (cap {MAX_CONFIG_BYTES})",
        text.len()
    );
    let bytes = text.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0usize;
    let mut in_string = false;
    let mut escaped = false;
    while i < bytes.len() {
        let c = bytes[i];
        if in_string {
            out.push(c);
            if escaped {
                escaped = false;
            } else if c == b'\\' {
                escaped = true;
            } else if c == b'"' {
                in_string = false;
            }
            i += 1;
            continue;
        }
        if c == b'"' {
            in_string = true;
            out.push(c);
            i += 1;
            continue;
        }
        if c == b'/' && bytes.get(i + 1) == Some(&b'/') {
            i += 2;
            while i < bytes.len() && bytes[i] != b'\n' {
                i += 1;
            }
            out.push(b' ');
            continue;
        }
        if c == b'/' && bytes.get(i + 1) == Some(&b'*') {
            i += 2;
            let mut closed = false;
            while i + 1 < bytes.len() {
                if bytes[i] == b'*' && bytes[i + 1] == b'/' {
                    i += 2;
                    closed = true;
                    break;
                }
                if bytes[i] == b'\n' {
                    out.push(b'\n');
                }
                i += 1;
            }
            anyhow::ensure!(closed, "unterminated block comment");
            out.push(b' ');
            continue;
        }
        out.push(c);
        i += 1;
    }
    anyhow::ensure!(!in_string, "unterminated string");
    // Trailing commas are legal JSONC, illegal JSON.
    let mut clean: Vec<u8> = Vec::with_capacity(out.len());
    let mut i = 0usize;
    let mut in_string = false;
    let mut escaped = false;
    while i < out.len() {
        let c = out[i];
        if in_string {
            clean.push(c);
            if escaped {
                escaped = false;
            } else if c == b'\\' {
                escaped = true;
            } else if c == b'"' {
                in_string = false;
            }
            i += 1;
            continue;
        }
        if c == b'"' {
            in_string = true;
            clean.push(c);
            i += 1;
            continue;
        }
        if c == b',' {
            let mut j = i + 1;
            while j < out.len() && matches!(out[j], b' ' | b'\t' | b'\r' | b'\n') {
                j += 1;
            }
            if j < out.len() && matches!(out[j], b'}' | b']') {
                i += 1;
                continue;
            }
        }
        clean.push(c);
        i += 1;
    }
    String::from_utf8(clean).map_err(Into::into)
}

/// Parse one JSONC document; the result must be an object.
pub fn parse_object(text: &str) -> Result<Value> {
    let cleaned = strip(text)?;
    let value: Value =
        serde_json::from_str(&cleaned).map_err(|e| anyhow::anyhow!("invalid JSON/JSONC: {e}"))?;
    anyhow::ensure!(value.is_object(), "config document must be a JSON object");
    Ok(value)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn comments_and_trailing_commas_are_stripped() {
        let v = parse_object(
            r#"{
                // line
                "url": "http://x/a//b",
                /* block */
                "list": [1, 2,],
            }"#,
        )
        .unwrap();
        assert_eq!(v["url"], "http://x/a//b");
        assert_eq!(v["list"], serde_json::json!([1, 2]));
    }

    #[test]
    fn unterminated_cases_are_errors() {
        assert!(strip("{ /* open").is_err());
        assert!(strip(r#"{"a": "open}"#).is_err());
    }
}
