//! Typed wire requests for the prompt family — the field-contract layer.
//!
//! The 2026-10-09 oc-remote double-prompt bug was a field-name divergence:
//! readers used `messageId` while the frozen v1 contract sends `messageID`
//! (`prompt.ts:1499 PromptInput`), so client-supplied ids were silently
//! dropped and the app rendered both its optimistic row and the server row.
//! This module closes that class: every prompt-family request body decodes
//! ONCE at the boundary into a typed struct whose serde/wire names are
//! compile-checked against the frozen spec (`bench/openapi/1.18.31.json`,
//! pinned test), with byte-exact Effect decode-error messages captured from
//! freeze in `bench/openapi/field-probes.md`.
//!
//! Contract rules (probe-derived, do not change without re-probing):
//! - unknown keys are ignored (upstream `onExcessProperty: ignore`),
//! - validation walks spec property order, first failure wins,
//! - root non-object body → `Expected object, got <js>` with NO `at` path,
//! - `at` path segments: keys quoted, array indexes bare (`["parts"][0]["text"]`),
//! - absent-optional ≠ present-null for REQUIRED fields (`parts:null` errors).

use serde_json::{Map, Value};
use std::fmt::Write as _;

/// Decode failure carrying the FULL Effect message (path included).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WireError {
    pub message: String,
}

impl WireError {
    fn new(expected: &str, got: Option<&Value>, path: &[Seg]) -> Self {
        let mut message = match got {
            Some(v) => format!("Expected {expected}, got {}", js(v)),
            None => "Missing key".to_string(),
        };
        if !path.is_empty() {
            message.push_str("\n  at ");
            message.push_str(&render_path(path));
        }
        Self { message }
    }
}

/// JS-compatible value rendering (Effect prints the raw JSON value):
/// `123`, `"x"`, `true`, `null`, `{}`, `{"type":"bogus"}`, `[1]`.
fn js(v: &Value) -> String {
    v.to_string()
}

#[derive(Clone)]
enum Seg {
    Key(&'static str),
    KeyDyn(String),
    Idx(usize),
}

fn render_path(path: &[Seg]) -> String {
    let mut out = String::new();
    for s in path {
        match s {
            Seg::Key(k) => {
                let _ = write!(out, "[\"{}\"]", k);
            }
            Seg::KeyDyn(k) => {
                let _ = write!(out, "[\"{}\"]", k);
            }
            Seg::Idx(i) => {
                let _ = write!(out, "[{}]", i);
            }
        }
    }
    out
}

fn root_not_object(v: &Value) -> WireError {
    WireError {
        message: format!("Expected object, got {}", js(v)),
    }
}

/// `Expected <t> | null, got <v>` for optional scalars / `Expected <t>, got <v>`
/// for required. `nullable` mirrors Effect's rendering of optional fields.
fn type_err(expected: &str, nullable: bool, v: &Value, path: &[Seg]) -> WireError {
    let exp = if nullable {
        format!("{expected} | null")
    } else {
        expected.to_string()
    };
    WireError::new(&exp, Some(v), path)
}

fn str_opt(v: &Value, path: &[Seg]) -> Result<Option<String>, WireError> {
    match v {
        Value::Null => Ok(None),
        Value::String(s) => Ok(Some(s.clone())),
        other => Err(type_err("string", true, other, path)),
    }
}

fn str_req(v: &Value, path: &[Seg]) -> Result<String, WireError> {
    match v {
        Value::String(s) => Ok(s.clone()),
        other => Err(type_err("string", false, other, path)),
    }
}

fn bool_opt(v: &Value, path: &[Seg]) -> Result<bool, WireError> {
    match v {
        Value::Null | Value::Bool(_) => Ok(v == &Value::Bool(true)),
        other => Err(type_err("boolean", true, other, path)),
    }
}

fn str_prefix(v: &Value, prefix: &str, path: &[Seg]) -> Result<Option<String>, WireError> {
    match v {
        Value::Null => Ok(None),
        Value::String(s) => {
            if s.starts_with(prefix) {
                Ok(Some(s.clone()))
            } else {
                Err(WireError::new(
                    &format!("a string starting with \"{prefix}\""),
                    Some(v),
                    path,
                ))
            }
        }
        other => Err(type_err("string", true, other, path)),
    }
}

/// `{providerID, modelID}` (required both, property order; extras ignored).
fn model_ref(v: &Value, path: &[Seg]) -> Result<Option<ModelRef>, WireError> {
    match v {
        Value::Null => Ok(None),
        Value::Object(map) => {
            let mut p2 = path.to_vec();
            p2.push(Seg::Key("providerID"));
            let provider_id = match map.get("providerID") {
                Some(x) => str_req(x, &p2)?,
                None => return Err(WireError::new("Missing key", None, &p2)),
            };
            p2.pop();
            p2.push(Seg::Key("modelID"));
            let model_id = match map.get("modelID") {
                Some(x) => str_req(x, &p2)?,
                None => return Err(WireError::new("Missing key", None, &p2)),
            };
            Ok(Some(ModelRef {
                provider_id,
                model_id,
            }))
        }
        other => Err(type_err("object", true, other, path)),
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct ModelRef {
    pub provider_id: String,
    pub model_id: String,
}

/// Upstream `Provider.parseModel` (`provider.ts:2080`): first `/` splits,
/// remainder rejoins. No slash → modelID empty.
pub fn parse_model_str(s: &str) -> ModelRef {
    match s.split_once('/') {
        Some((p, m)) => ModelRef {
            provider_id: p.to_string(),
            model_id: m.to_string(),
        },
        None => ModelRef {
            provider_id: s.to_string(),
            model_id: String::new(),
        },
    }
}

// ---------------------------------------------------------------------------
// PromptRequest — v1 POST /session/{id}/message + /prompt_async
// ---------------------------------------------------------------------------

/// Frozen `PromptInput` (prompt.ts:1499) minus sessionID (path). Wire keys are
/// the EXACT spec property names; `wire_keys()` is pinned to the spec by test.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct PromptRequest {
    pub message_id: Option<String>,
    pub model: Option<ModelRef>,
    pub agent: Option<String>,
    pub no_reply: bool,
    pub tools: Option<Map<String, Value>>,
    pub format: Option<Value>,
    pub system: Option<String>,
    pub variant: Option<String>,
    pub parts: Vec<Value>,
}

impl PromptRequest {
    pub const WIRE_KEYS: [&'static str; 9] = [
        "messageID",
        "model",
        "agent",
        "noReply",
        "tools",
        "format",
        "system",
        "variant",
        "parts",
    ];

    pub fn decode(v: &Value) -> Result<Self, WireError> {
        let Value::Object(map) = v else {
            return Err(root_not_object(v));
        };
        let mut path: Vec<Seg> = Vec::new();
        let mut req = PromptRequest::default();

        // property order: messageID, model, agent, noReply, tools, format,
        // system, variant, parts (first failure wins — probes P-order).
        if let Some(x) = map.get("messageID") {
            path.push(Seg::Key("messageID"));
            req.message_id = str_prefix(x, "msg", &path)?;
            path.pop();
        }
        if let Some(x) = map.get("model") {
            path.push(Seg::Key("model"));
            req.model = model_ref(x, &path)?;
            path.pop();
        }
        if let Some(x) = map.get("agent") {
            path.push(Seg::Key("agent"));
            req.agent = str_opt(x, &path)?;
            path.pop();
        }
        if let Some(x) = map.get("noReply") {
            path.push(Seg::Key("noReply"));
            req.no_reply = bool_opt(x, &path)?;
            path.pop();
        }
        if let Some(x) = map.get("tools") {
            path.push(Seg::Key("tools"));
            match x {
                Value::Null => {}
                Value::Object(tm) => {
                    let mut tmap = Map::new();
                    for (k, tv) in tm {
                        let mut p2 = path.clone();
                        p2.push(Seg::KeyDyn(k.clone()));
                        if !matches!(tv, Value::Bool(_)) {
                            return Err(type_err("boolean", false, tv, &p2));
                        }
                        tmap.insert(k.clone(), tv.clone());
                    }
                    req.tools = Some(tmap);
                }
                other => return Err(type_err("object", true, other, &path)),
            }
            path.pop();
        }
        if let Some(x) = map.get("format") {
            path.push(Seg::Key("format"));
            // null = absent (consistent with every other optional — format:null
            // itself is unprobed; treat like the probed optionals).
            if !x.is_null() {
                req.format = Some(decode_format(x, &path)?);
            }
            path.pop();
        }
        if let Some(x) = map.get("system") {
            path.push(Seg::Key("system"));
            req.system = str_opt(x, &path)?;
            path.pop();
        }
        if let Some(x) = map.get("variant") {
            path.push(Seg::Key("variant"));
            req.variant = str_opt(x, &path)?;
            path.pop();
        }
        // REQUIRED: absent → Missing key; null → Expected array, got null.
        path.push(Seg::Key("parts"));
        match map.get("parts") {
            None => return Err(WireError::new("Missing key", None, &path)),
            Some(Value::Array(arr)) => {
                for (i, el) in arr.iter().enumerate() {
                    path.push(Seg::Idx(i));
                    req.parts.push(decode_part(el, &path)?);
                    path.pop();
                }
            }
            Some(other) => return Err(type_err("array", false, other, &path)),
        }
        path.pop();
        Ok(req)
    }
}

/// `OutputFormat` union (probe-pinned): non-object non-null →
/// `Expected OutputFormat | null`; object with missing/unknown `type` →
/// `Expected OutputFormat`; json_schema branch requires `schema`.
fn decode_format(v: &Value, path: &[Seg]) -> Result<Value, WireError> {
    match v {
        Value::Null => Ok(Value::Null),
        Value::Object(map) => {
            let t = map.get("type").and_then(|t| t.as_str());
            match t {
                Some("text") => Ok(v.clone()),
                Some("json_schema") => {
                    let mut p2 = path.to_vec();
                    p2.push(Seg::Key("schema"));
                    match map.get("schema") {
                        None => Err(WireError::new("Missing key", None, &p2)),
                        Some(s) if s.is_object() => {
                            if let Some(rc) = map.get("retryCount")
                                && !rc.is_number()
                            {
                                p2.pop();
                                p2.push(Seg::Key("retryCount"));
                                // Effect renders this optional integer
                                // with a doubled null union — probed byte.
                                return Err(WireError::new("number | null | null", Some(rc), &p2));
                            }
                            Ok(v.clone())
                        }
                        Some(s) => Err(type_err("JSONSchema", false, s, &p2)),
                    }
                }
                _ => Err(type_err("OutputFormat", false, v, path)),
            }
        }
        other => Err(type_err("OutputFormat", true, other, path)),
    }
}

/// Four-way part union discriminant (`{ readonly "type": "text", ... } | …`).
const PART_UNION: &str = concat!(
    r#"{ readonly "type": "text", ... } | { readonly "type": "file", ... } | "#,
    r#"{ readonly "type": "agent", ... } | { readonly "type": "subtask", ... }"#
);
const FILE_UNION: &str = r#"{ readonly "type": "file", ... }"#;

/// Validate one v1 message part. Discriminate on `type` first (missing or
/// unknown → union error with the whole object), then property-order checks.
fn decode_part(v: &Value, path: &[Seg]) -> Result<Value, WireError> {
    let Value::Object(map) = v else {
        return Err(WireError::new(PART_UNION, Some(v), path));
    };
    let t = map.get("type").and_then(|t| t.as_str());
    match t {
        Some("text") => {
            decode_part_id(map, path)?;
            if let Some(x) = map.get("synthetic") {
                let mut p2 = path.to_vec();
                p2.push(Seg::Key("synthetic"));
                bool_opt(x, &p2)?;
            }
            if let Some(x) = map.get("ignored") {
                let mut p2 = path.to_vec();
                p2.push(Seg::Key("ignored"));
                bool_opt(x, &p2)?;
            }
            let mut p2 = path.to_vec();
            p2.push(Seg::Key("text"));
            match map.get("text") {
                None => return Err(WireError::new("Missing key", None, &p2)),
                Some(x) => {
                    str_req(x, &p2)?;
                }
            }
        }
        Some("file") => {
            decode_part_id(map, path)?;
            if let Some(x) = map.get("filename") {
                let mut p2 = path.to_vec();
                p2.push(Seg::Key("filename"));
                str_opt(x, &p2)?;
            }
            let mut p2 = path.to_vec();
            p2.push(Seg::Key("mime"));
            match map.get("mime") {
                None => return Err(WireError::new("Missing key", None, &p2)),
                Some(x) => {
                    str_req(x, &p2)?;
                }
            }
            p2.pop();
            p2.push(Seg::Key("url"));
            match map.get("url") {
                None => return Err(WireError::new("Missing key", None, &p2)),
                Some(x) => {
                    str_req(x, &p2)?;
                }
            }
        }
        Some("agent") => {
            decode_part_id(map, path)?;
            let mut p2 = path.to_vec();
            p2.push(Seg::Key("name"));
            match map.get("name") {
                None => return Err(WireError::new("Missing key", None, &p2)),
                Some(x) => {
                    str_req(x, &p2)?;
                }
            }
        }
        Some("subtask") => {
            decode_part_id(map, path)?;
            // required order (probe): prompt, description, agent
            for k in ["prompt", "description", "agent"] {
                let mut p2 = path.to_vec();
                p2.push(Seg::Key(k));
                match map.get(k) {
                    None => return Err(WireError::new("Missing key", None, &p2)),
                    Some(x) => {
                        str_req(x, &p2)?;
                    }
                }
            }
        }
        _ => return Err(WireError::new(PART_UNION, Some(v), path)),
    }
    Ok(v.clone())
}

/// Optional `id` on parts: `^prt` pattern when a string (probe: id checked
/// before sibling required keys = spec property order).
fn decode_part_id(map: &Map<String, Value>, path: &[Seg]) -> Result<(), WireError> {
    if let Some(x) = map.get("id") {
        let mut p2 = path.to_vec();
        p2.push(Seg::Key("id"));
        str_prefix(x, "prt", &p2)?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// CommandRequest — v1 POST /session/{id}/command
// ---------------------------------------------------------------------------

/// Frozen `CommandInput` (prompt.ts:1536): required `[arguments, command]`,
/// `model` is a STRING (`providerID/modelID`, parsed by `parse_model_str`),
/// `parts` optional and FILE-only.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct CommandRequest {
    pub message_id: Option<String>,
    pub agent: Option<String>,
    /// raw wire string — callers parse via `parse_model_str`
    pub model: Option<String>,
    pub arguments: String,
    pub command: String,
    pub variant: Option<String>,
    pub parts: Vec<Value>,
}

impl CommandRequest {
    pub const WIRE_KEYS: [&'static str; 7] = [
        "messageID",
        "agent",
        "model",
        "arguments",
        "command",
        "variant",
        "parts",
    ];

    pub fn decode(v: &Value) -> Result<Self, WireError> {
        let Value::Object(map) = v else {
            return Err(root_not_object(v));
        };
        let mut path: Vec<Seg> = Vec::new();
        let mut req = CommandRequest::default();
        if let Some(x) = map.get("messageID") {
            path.push(Seg::Key("messageID"));
            req.message_id = str_prefix(x, "msg", &path)?;
            path.pop();
        }
        if let Some(x) = map.get("agent") {
            path.push(Seg::Key("agent"));
            req.agent = str_opt(x, &path)?;
            path.pop();
        }
        if let Some(x) = map.get("model") {
            path.push(Seg::Key("model"));
            req.model = str_opt(x, &path)?;
            path.pop();
        }
        path.push(Seg::Key("arguments"));
        match map.get("arguments") {
            None => return Err(WireError::new("Missing key", None, &path)),
            Some(x) => {
                req.arguments = str_req(x, &path)?;
            }
        }
        path.pop();
        path.push(Seg::Key("command"));
        match map.get("command") {
            None => return Err(WireError::new("Missing key", None, &path)),
            Some(x) => {
                req.command = str_req(x, &path)?;
            }
        }
        path.pop();
        if let Some(x) = map.get("variant") {
            path.push(Seg::Key("variant"));
            req.variant = str_opt(x, &path)?;
            path.pop();
        }
        if let Some(x) = map.get("parts") {
            path.push(Seg::Key("parts"));
            match x {
                Value::Null => {}
                Value::Array(arr) => {
                    for (i, el) in arr.iter().enumerate() {
                        path.push(Seg::Idx(i));
                        req.parts.push(decode_file_part(el, &path)?);
                        path.pop();
                    }
                }
                other => return Err(type_err("array", true, other, &path)),
            }
            path.pop();
        }
        Ok(req)
    }
}

/// Command parts are file-only (probe: text part → `Expected { readonly
/// "type": "file", ... }`).
fn decode_file_part(v: &Value, path: &[Seg]) -> Result<Value, WireError> {
    let Value::Object(map) = v else {
        return Err(WireError::new(FILE_UNION, Some(v), path));
    };
    match map.get("type").and_then(|t| t.as_str()) {
        Some("file") => {
            decode_part_id(map, path)?;
            if let Some(x) = map.get("filename") {
                let mut p2 = path.to_vec();
                p2.push(Seg::Key("filename"));
                str_opt(x, &p2)?;
            }
            let mut p2 = path.to_vec();
            p2.push(Seg::Key("mime"));
            match map.get("mime") {
                None => return Err(WireError::new("Missing key", None, &p2)),
                Some(x) => {
                    str_req(x, &p2)?;
                }
            }
            p2.pop();
            p2.push(Seg::Key("url"));
            match map.get("url") {
                None => return Err(WireError::new("Missing key", None, &p2)),
                Some(x) => {
                    str_req(x, &p2)?;
                }
            }
            Ok(v.clone())
        }
        _ => Err(WireError::new(FILE_UNION, Some(v), path)),
    }
}

// ---------------------------------------------------------------------------
// ShellRequest — v1 POST /session/{id}/shell
// ---------------------------------------------------------------------------

/// Frozen `ShellInput`: required `[agent, command]` (probe: `{}` → agent),
/// `model` is an OBJECT, optional `messageID` `^msg`.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ShellRequest {
    pub message_id: Option<String>,
    pub agent: String,
    pub model: Option<ModelRef>,
    pub command: String,
}

impl ShellRequest {
    pub const WIRE_KEYS: [&'static str; 4] = ["messageID", "agent", "model", "command"];

    pub fn decode(v: &Value) -> Result<Self, WireError> {
        let Value::Object(map) = v else {
            return Err(root_not_object(v));
        };
        let mut path: Vec<Seg> = Vec::new();
        let mut req = ShellRequest::default();
        if let Some(x) = map.get("messageID") {
            path.push(Seg::Key("messageID"));
            req.message_id = str_prefix(x, "msg", &path)?;
            path.pop();
        }
        path.push(Seg::Key("agent"));
        match map.get("agent") {
            None => return Err(WireError::new("Missing key", None, &path)),
            Some(x) => {
                req.agent = str_req(x, &path)?;
            }
        }
        path.pop();
        if let Some(x) = map.get("model") {
            path.push(Seg::Key("model"));
            req.model = model_ref(x, &path)?;
            path.pop();
        }
        path.push(Seg::Key("command"));
        match map.get("command") {
            None => return Err(WireError::new("Missing key", None, &path)),
            Some(x) => {
                req.command = str_req(x, &path)?;
            }
        }
        path.pop();
        Ok(req)
    }
}

// ---------------------------------------------------------------------------
// V2PromptInput — POST /api/session/{sessionID}/prompt
// ---------------------------------------------------------------------------

/// Frozen v2 body: required `[prompt]`; `id` is `^msg_` (UNDERSCORE — differs
/// from v1 `^msg`, probe-pinned); delivery enum; resume boolean.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct V2PromptInput {
    pub id: Option<String>,
    pub prompt_text: String,
    pub prompt_files: Vec<Value>,
    pub prompt_agents: Vec<Value>,
    pub delivery: Option<String>,
    pub resume: Option<bool>,
}

impl V2PromptInput {
    pub const WIRE_KEYS: [&'static str; 4] = ["id", "prompt", "delivery", "resume"];

    pub fn decode(v: &Value) -> Result<Self, WireError> {
        let Value::Object(map) = v else {
            return Err(root_not_object(v));
        };
        let mut path: Vec<Seg> = Vec::new();
        let mut req = V2PromptInput::default();
        if let Some(x) = map.get("id") {
            path.push(Seg::Key("id"));
            req.id = str_prefix(x, "msg_", &path)?;
            path.pop();
        }
        path.push(Seg::Key("prompt"));
        let pobj = match map.get("prompt") {
            None => return Err(WireError::new("Missing key", None, &path)),
            Some(Value::Object(pm)) => pm,
            Some(other) => {
                return Err(type_err("PromptInput", false, other, &path));
            }
        };
        path.push(Seg::Key("text"));
        match pobj.get("text") {
            None => return Err(WireError::new("Missing key", None, &path)),
            Some(x) => {
                req.prompt_text = str_req(x, &path)?;
            }
        }
        path.pop();
        if let Some(x) = pobj.get("files") {
            path.push(Seg::Key("files"));
            match x {
                Value::Array(arr) => {
                    for (i, el) in arr.iter().enumerate() {
                        path.push(Seg::Idx(i));
                        let Value::Object(fm) = el else {
                            return Err(WireError::new(
                                "PromptInput.FileAttachment",
                                Some(el),
                                &path,
                            ));
                        };
                        path.push(Seg::Key("uri"));
                        match fm.get("uri") {
                            None => return Err(WireError::new("Missing key", None, &path)),
                            Some(u) => {
                                str_req(u, &path)?;
                            }
                        }
                        path.pop();
                        req.prompt_files.push(el.clone());
                        path.pop(); // Idx
                    }
                }
                other => return Err(type_err("array", true, other, &path)),
            }
            path.pop();
        }
        if let Some(x) = pobj.get("agents") {
            path.push(Seg::Key("agents"));
            match x {
                Value::Array(arr) => {
                    for (i, el) in arr.iter().enumerate() {
                        path.push(Seg::Idx(i));
                        let Value::Object(am) = el else {
                            return Err(WireError::new("Prompt.AgentAttachment", Some(el), &path));
                        };
                        path.push(Seg::Key("name"));
                        match am.get("name") {
                            None => return Err(WireError::new("Missing key", None, &path)),
                            Some(u) => {
                                str_req(u, &path)?;
                            }
                        }
                        path.pop();
                        req.prompt_agents.push(el.clone());
                        path.pop();
                    }
                }
                other => return Err(type_err("array", true, other, &path)),
            }
            path.pop();
        }
        path.pop(); // prompt
        if let Some(x) = map.get("delivery") {
            path.push(Seg::Key("delivery"));
            match x {
                Value::Null => {}
                Value::String(s) if s == "steer" || s == "queue" => {
                    req.delivery = Some(s.clone());
                }
                other => {
                    return Err(WireError::new(r#""steer" | "queue""#, Some(other), &path));
                }
            }
            path.pop();
        }
        if let Some(x) = map.get("resume") {
            path.push(Seg::Key("resume"));
            req.resume = match x {
                Value::Null => None,
                Value::Bool(b) => Some(*b),
                other => return Err(type_err("boolean", true, other, &path)),
            };
            path.pop();
        }
        Ok(req)
    }
}

// ---------------------------------------------------------------------------
// Probe corpus — byte-exact freeze captures (field-probes.md is the source)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod wire_tests {
    use super::*;
    use serde_json::json;

    fn err(v: &Value) -> String {
        match PromptRequest::decode(v) {
            Ok(_) => panic!("expected decode failure for {v}"),
            Err(e) => e.message,
        }
    }

    #[test]
    fn probe_root_not_object() {
        assert_eq!(err(&json!([1])), "Expected object, got [1]");
        assert_eq!(err(&json!("hello")), r#"Expected object, got "hello""#);
    }

    #[test]
    fn probe_parts_required_and_null() {
        assert_eq!(err(&json!({})), "Missing key\n  at [\"parts\"]");
        assert_eq!(
            err(&json!({"parts": null})),
            "Expected array, got null\n  at [\"parts\"]"
        );
        assert_eq!(
            err(&json!({"parts": "x"})),
            "Expected array, got \"x\"\n  at [\"parts\"]"
        );
    }

    #[test]
    fn probe_messageid_rules() {
        assert_eq!(
            err(&json!({"messageID": 123, "parts": []})),
            "Expected string | null, got 123\n  at [\"messageID\"]"
        );
        assert_eq!(
            err(&json!({"messageID": true, "parts": []})),
            "Expected string | null, got true\n  at [\"messageID\"]"
        );
        assert_eq!(
            err(&json!({"messageID": "notamsg", "parts": []})),
            r#"Expected a string starting with "msg", got "notamsg""#.to_string()
                + "\n  at [\"messageID\"]"
        );
        // bare "msg" passes (startsWith only), null passes, 5000 chars pass
        assert!(PromptRequest::decode(&json!({"messageID": "msg", "parts": []})).is_ok());
        assert!(PromptRequest::decode(&json!({"messageID": null, "parts": []})).is_ok());
        let long = format!("msg_{}", "A".repeat(5000));
        assert!(PromptRequest::decode(&json!({"messageID": long, "parts": []})).is_ok());
        // ordering: messageID before model
        assert!(err(&json!({"messageID": "bad", "model": {}})).contains("[\"messageID\"]"));
        // ordering: agent before parts
        assert!(err(&json!({"agent": 123, "parts": "x"})).contains("[\"agent\"]"));
    }

    #[test]
    fn probe_model_rules() {
        assert_eq!(
            err(&json!({"parts": [], "model": 123})),
            "Expected object | null, got 123\n  at [\"model\"]"
        );
        assert_eq!(
            err(&json!({"parts": [], "model": {}})),
            "Missing key\n  at [\"model\"][\"providerID\"]"
        );
        assert_eq!(
            err(&json!({"parts": [], "model": {"providerID": "x"}})),
            "Missing key\n  at [\"model\"][\"modelID\"]"
        );
        assert_eq!(
            err(&json!({"parts": [], "model": {"providerID": 123, "modelID": "x"}})),
            "Expected string, got 123\n  at [\"model\"][\"providerID\"]"
        );
        // extras ignored
        assert!(
            PromptRequest::decode(
                &json!({"parts": [], "model": {"providerID":"fake","modelID":"m","zzz":1}})
            )
            .is_ok()
        );
    }

    #[test]
    fn probe_scalar_optionals() {
        assert_eq!(
            err(&json!({"parts": [], "agent": 123})),
            "Expected string | null, got 123\n  at [\"agent\"]"
        );
        assert_eq!(
            err(&json!({"parts": [], "noReply": "x"})),
            r#"Expected boolean | null, got "x""#.to_string() + "\n  at [\"noReply\"]"
        );
        assert_eq!(
            err(&json!({"parts": [], "system": 123})),
            "Expected string | null, got 123\n  at [\"system\"]"
        );
        assert_eq!(
            err(&json!({"parts": [], "variant": 123})),
            "Expected string | null, got 123\n  at [\"variant\"]"
        );
        // unknown top-level key ignored
        assert!(
            PromptRequest::decode(&json!({"messageId": "msg_shouldbeignored", "parts": []}))
                .is_ok()
        );
    }

    #[test]
    fn probe_tools_rules() {
        assert_eq!(
            err(&json!({"parts": [], "tools": "x"})),
            r#"Expected object | null, got "x""#.to_string() + "\n  at [\"tools\"]"
        );
        assert_eq!(
            err(&json!({"parts": [], "tools": {"a": 1}})),
            "Expected boolean, got 1\n  at [\"tools\"][\"a\"]"
        );
        assert!(PromptRequest::decode(&json!({"parts": [], "tools": null})).is_ok());
        assert!(
            PromptRequest::decode(&json!({"parts": [], "tools": {"bash": false, "edit": true}}))
                .is_ok()
        );
    }

    #[test]
    fn probe_format_rules() {
        assert_eq!(
            err(&json!({"parts": [], "format": 123})),
            "Expected OutputFormat | null, got 123\n  at [\"format\"]"
        );
        assert_eq!(
            err(&json!({"parts": [], "format": "text"})),
            r#"Expected OutputFormat | null, got "text""#.to_string() + "\n  at [\"format\"]"
        );
        assert_eq!(
            err(&json!({"parts": [], "format": {}})),
            "Expected OutputFormat, got {}\n  at [\"format\"]"
        );
        assert_eq!(
            err(&json!({"parts": [], "format": {"type": "bogus"}})),
            r#"Expected OutputFormat, got {"type":"bogus"}"#.to_string() + "\n  at [\"format\"]"
        );
        assert_eq!(
            err(&json!({"parts": [], "format": {"type": "json_schema"}})),
            "Missing key\n  at [\"format\"][\"schema\"]"
        );
        assert_eq!(
            err(&json!({"parts": [], "format": {"type":"json_schema","schema":123}})),
            "Expected JSONSchema, got 123\n  at [\"format\"][\"schema\"]"
        );
        assert_eq!(
            err(
                &json!({"parts": [], "format": {"type":"json_schema","schema":{},"retryCount":"x"}})
            ),
            "Expected number | null | null, got \"x\"\n  at [\"format\"][\"retryCount\"]"
        );
        assert!(PromptRequest::decode(&json!({"parts": [], "format": {"type": "text"}})).is_ok());
    }

    #[test]
    fn probe_part_union_and_fields() {
        assert_eq!(
            err(&json!({"parts": [123]})),
            format!("Expected {PART_UNION}, got 123\n  at [\"parts\"][0]")
        );
        assert_eq!(
            err(&json!({"parts": [{}]})),
            format!("Expected {PART_UNION}, got {{}}\n  at [\"parts\"][0]")
        );
        assert_eq!(
            err(&json!({"parts": [{"type": "bogus"}]})),
            format!(r#"Expected {PART_UNION}, got {{"type":"bogus"}}"#,) + "\n  at [\"parts\"][0]"
        );
        assert_eq!(
            err(&json!({"parts": [{"type": "text"}]})),
            "Missing key\n  at [\"parts\"][0][\"text\"]"
        );
        assert_eq!(
            err(&json!({"parts": [{"type": "text", "text": 123}]})),
            "Expected string, got 123\n  at [\"parts\"][0][\"text\"]"
        );
        assert_eq!(
            err(&json!({"parts": [{"type": "text", "text": "x", "id": "bad"}]})),
            r#"Expected a string starting with "prt", got "bad""#.to_string()
                + "\n  at [\"parts\"][0][\"id\"]"
        );
        assert_eq!(
            err(&json!({"parts": [{"type": "text", "text": "x", "synthetic": "bad"}]})),
            "Expected boolean | null, got \"bad\"\n  at [\"parts\"][0][\"synthetic\"]"
        );
        // file required order mime → url
        assert_eq!(
            err(&json!({"parts": [{"type": "file"}]})),
            "Missing key\n  at [\"parts\"][0][\"mime\"]"
        );
        assert_eq!(
            err(&json!({"parts": [{"type": "file", "mime": "a/b", "filename": "a"}]})),
            "Missing key\n  at [\"parts\"][0][\"url\"]"
        );
        // agent: name
        assert_eq!(
            err(&json!({"parts": [{"type": "agent"}]})),
            "Missing key\n  at [\"parts\"][0][\"name\"]"
        );
        // subtask: prompt → description → agent
        assert_eq!(
            err(&json!({"parts": [{"type": "subtask"}]})),
            "Missing key\n  at [\"parts\"][0][\"prompt\"]"
        );
        assert_eq!(
            err(&json!({"parts": [{"type": "subtask", "prompt": "p"}]})),
            "Missing key\n  at [\"parts\"][0][\"description\"]"
        );
        assert_eq!(
            err(&json!({"parts": [{"type": "subtask", "prompt": "p", "description": "d"}]})),
            "Missing key\n  at [\"parts\"][0][\"agent\"]"
        );
        // extra keys on a valid part pass
        assert!(
            PromptRequest::decode(&json!({
                "parts": [{"type": "text", "text": "x", "bogus": 1, "sessionID": "ses_z"}]
            }))
            .is_ok()
        );
    }

    #[test]
    fn probe_valid_bodies() {
        let ok = PromptRequest::decode(&json!({
            "messageID": "msg_client_0000000000000000000001",
            "model": {"providerID": "fake", "modelID": "m"},
            "agent": "build",
            "noReply": false,
            "tools": {"bash": true},
            "system": "sys",
            "variant": "high",
            "parts": [{"type": "text", "text": "hi"}],
        }))
        .expect("full valid body");
        assert_eq!(
            ok.message_id.as_deref(),
            Some("msg_client_0000000000000000000001")
        );
        assert_eq!(ok.parts.len(), 1);
        // lowercase messageId is IGNORED (ignore-unknown-keys pin)
        let ignored = PromptRequest::decode(
            &json!({"messageId": "msg_lowercase000000000000000001", "parts": []}),
        )
        .unwrap();
        assert_eq!(ignored.message_id, None);
        // noReply true passes
        let nr = PromptRequest::decode(
            &json!({"noReply": true, "parts": [{"type":"text","text":"hi"}]}),
        )
        .unwrap();
        assert!(nr.no_reply);
    }

    #[test]
    fn probe_command_decode() {
        assert_eq!(
            CommandRequest::decode(&json!({})).unwrap_err().message,
            "Missing key\n  at [\"arguments\"]"
        );
        assert_eq!(
            CommandRequest::decode(&json!({"command": "x"}))
                .unwrap_err()
                .message,
            "Missing key\n  at [\"arguments\"]"
        );
        assert_eq!(
            CommandRequest::decode(&json!({"arguments": "x"}))
                .unwrap_err()
                .message,
            "Missing key\n  at [\"command\"]"
        );
        assert_eq!(
            CommandRequest::decode(&json!({"messageID": "bad"}))
                .unwrap_err()
                .message,
            "Expected a string starting with \"msg\", got \"bad\"\n  at [\"messageID\"]"
        );
        // command parts are FILE-only (probe: text part rejected)
        let e = CommandRequest::decode(&json!({
            "command": "x", "arguments": "y",
            "parts": [{"type": "text", "text": "z"}]
        }))
        .unwrap_err()
        .message;
        assert_eq!(
            e,
            format!(r#"Expected {FILE_UNION}, got {{"type":"text","text":"z"}}"#)
                + "\n  at [\"parts\"][0]"
        );
        // file part + extra key + absent parts all pass
        assert!(
            CommandRequest::decode(&json!({
                "command": "noop", "arguments": "x", "extra": 1,
                "parts": [{"type": "file", "mime": "a/b", "url": "u"}]
            }))
            .is_ok()
        );
        assert!(CommandRequest::decode(&json!({"command": "noop", "arguments": ""})).is_ok());
        // model is a string on the command wire
        let c = CommandRequest::decode(&json!({"command":"x","arguments":"y","model":"fake/m"}))
            .unwrap();
        assert_eq!(c.model.as_deref(), Some("fake/m"));
        assert_eq!(
            parse_model_str("fake/m"),
            ModelRef {
                provider_id: "fake".into(),
                model_id: "m".into()
            }
        );
        assert_eq!(
            parse_model_str("a/b/c"),
            ModelRef {
                provider_id: "a".into(),
                model_id: "b/c".into()
            }
        );
        assert_eq!(
            parse_model_str("nomodel"),
            ModelRef {
                provider_id: "nomodel".into(),
                model_id: "".into()
            }
        );
    }

    #[test]
    fn probe_shell_decode() {
        assert_eq!(
            ShellRequest::decode(&json!({})).unwrap_err().message,
            "Missing key\n  at [\"agent\"]"
        );
        assert_eq!(
            ShellRequest::decode(&json!({"command": "ls"}))
                .unwrap_err()
                .message,
            "Missing key\n  at [\"agent\"]"
        );
        let s = ShellRequest::decode(&json!({
            "messageID": "msg_shell000000000000000000001",
            "agent": "build",
            "model": {"providerID": "p", "modelID": "m"},
            "command": "ls"
        }))
        .unwrap();
        assert_eq!(s.agent, "build");
        assert_eq!(s.model.as_ref().unwrap().provider_id, "p");
    }

    #[test]
    fn probe_v2_decode() {
        let v2err = |v: &Value| V2PromptInput::decode(v).unwrap_err().message;
        assert_eq!(v2err(&json!({})), "Missing key\n  at [\"prompt\"]");
        assert_eq!(
            v2err(&json!({"prompt": "x"})),
            r#"Expected PromptInput, got "x""#.to_string() + "\n  at [\"prompt\"]"
        );
        assert_eq!(
            v2err(&json!({"prompt": {"files": []}})),
            "Missing key\n  at [\"prompt\"][\"text\"]"
        );
        assert_eq!(
            v2err(&json!({"prompt": {"text": 123}})),
            "Expected string, got 123\n  at [\"prompt\"][\"text\"]"
        );
        assert_eq!(
            v2err(&json!({"prompt": {"text": "x", "files": [{}]}})),
            "Missing key\n  at [\"prompt\"][\"files\"][0][\"uri\"]"
        );
        assert_eq!(
            v2err(&json!({"prompt": {"text": "x", "files": [123]}})),
            r#"Expected PromptInput.FileAttachment, got 123"#.to_string()
                + "\n  at [\"prompt\"][\"files\"][0]"
        );
        assert_eq!(
            v2err(&json!({"prompt": {"text": "x", "agents": [123]}})),
            r#"Expected Prompt.AgentAttachment, got 123"#.to_string()
                + "\n  at [\"prompt\"][\"agents\"][0]"
        );
        assert_eq!(
            v2err(&json!({"prompt": {"text": "x", "agents": [{}]}})),
            "Missing key\n  at [\"prompt\"][\"agents\"][0][\"name\"]"
        );
        // v2 id needs the UNDERSCORE prefix
        for bad in ["bad", "msgx"] {
            assert_eq!(
                v2err(&json!({"id": bad, "prompt": {"text": "x"}})),
                format!(r#"Expected a string starting with "msg_", got "{bad}""#)
                    + "\n  at [\"id\"]"
            );
        }
        assert_eq!(
            v2err(&json!({"prompt": {"text": "x"}, "delivery": "bogus"})),
            r#"Expected "steer" | "queue", got "bogus""#.to_string() + "\n  at [\"delivery\"]"
        );
        assert_eq!(
            v2err(&json!({"prompt": {"text": "x"}, "resume": "bad"})),
            r#"Expected boolean | null, got "bad""#.to_string() + "\n  at [\"resume\"]"
        );
        // valid: id, delivery null, files+agents
        let ok = V2PromptInput::decode(&json!({
            "id": null,
            "prompt": {"text": "x", "files": [{"uri": "file:///a"}], "agents": [{"name": "sub"}]},
            "delivery": null,
        }))
        .unwrap();
        assert_eq!(ok.id, None);
        assert_eq!(ok.prompt_text, "x");
        assert_eq!(ok.prompt_files.len(), 1);
        assert_eq!(ok.prompt_agents.len(), 1);
        let q =
            V2PromptInput::decode(&json!({"prompt": {"text": "x"}, "delivery": "queue"})).unwrap();
        assert_eq!(q.delivery.as_deref(), Some("queue"));
    }

    /// B4: wire key sets are pinned to the frozen spec. Generated from
    /// `bench/openapi/1.18.31.json` — upstream adds/renames a field → spec
    /// refresh → red here → struct updated in lockstep.
    #[test]
    fn wire_keys_match_frozen_spec() {
        let raw = include_str!("../../../bench/openapi/1.18.31.json");
        let spec: Value = serde_json::from_str(raw).expect("spec parses");
        let props = |path: &str, method: &str| -> Vec<String> {
            // Direct indexing only: serde_json::pointer tokenizes on `/`, and
            // both the spec path keys and the `application/json` content-type
            // key contain literal slashes.
            spec.get("paths")
                .and_then(|p| p.get(format!("/{path}")))
                .and_then(|op| op.get(method))
                .and_then(|m| m.get("requestBody"))
                .and_then(|rb| rb.get("content"))
                .and_then(|c| c.get("application/json"))
                .and_then(|ct| ct.get("schema"))
                .and_then(|s| s.get("properties"))
                .and_then(|p| p.as_object())
                .map(|m| m.keys().cloned().collect())
                .unwrap_or_else(|| panic!("no properties for {path} {method}"))
        };
        let mut msg = props("session/{sessionID}/message", "post");
        msg.retain(|k| k != "sessionID"); // sessionID is path-bound, not body
        assert_eq!(
            msg,
            PromptRequest::WIRE_KEYS
                .iter()
                .map(|s| s.to_string())
                .collect::<Vec<_>>()
        );
        let cmd = props("session/{sessionID}/command", "post");
        assert_eq!(
            cmd,
            CommandRequest::WIRE_KEYS
                .iter()
                .map(|s| s.to_string())
                .collect::<Vec<_>>()
        );
        let shell = props("session/{sessionID}/shell", "post");
        assert_eq!(
            shell,
            ShellRequest::WIRE_KEYS
                .iter()
                .map(|s| s.to_string())
                .collect::<Vec<_>>()
        );
        let v2 = props("api/session/{sessionID}/prompt", "post");
        assert_eq!(
            v2,
            V2PromptInput::WIRE_KEYS
                .iter()
                .map(|s| s.to_string())
                .collect::<Vec<_>>()
        );
    }
}
