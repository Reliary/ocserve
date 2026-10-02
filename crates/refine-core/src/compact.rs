//! Compaction-lite (W3): port of v1 `buildPrompt` + `serialize` from
//! `packages/core/src/session/compaction.ts` + `session/compaction.ts`
//! (source-derived; no live-mutation probe needed for prompt text).
//!
//! Divergences (documented, TESTING §1.6): no pruning/overflow machinery,
//! no compaction-state messages (the summary lands as a plain assistant
//! message), full history serialized (v1 prunes to token budgets), the
//! agent system prompt stays attached (v1 compaction shapes its own).

use serde_json::Value;

const TOOL_OUTPUT_MAX_CHARS: usize = 2_000;
const INSTRUCTION: &str = "Create a new anchored summary from the conversation history in the <conversation> tags above so another coding agent can continue the work.";

/// v1 `SUMMARY_TEMPLATE` verbatim (core compaction.ts:16-55).
pub const SUMMARY_TEMPLATE: &str = r#"Output exactly the Markdown structure shown inside <template> and keep the section order unchanged. Do not include the <template> tags in your response.
<template>
## Objective
- [one or two brief sentences describing what the user is trying to accomplish]

## Important Details
- [constraints/preferences, decisions and why, important facts/assumptions, exact context needed to continue, or "(none)"]

## Work State
### Completed
- [finished work, verified facts, or changes made; otherwise "(none)"]

### Active
- [current work, partial changes, or investigation state; otherwise "(none)"]

### Blocked
- [blockers, failing commands, or unknowns; otherwise "(none)"]

## Next Move
1. [immediate concrete action, or "(none)"]
2. [next action if known, or "(none)"]

## Relevant Files
- [file or directory path: why it matters, or "(none)"]
</template>

Rules:
- Keep every section, even when empty.
- Use terse bullets, not prose paragraphs.
- Preserve exact file paths, symbols, commands, error strings, URLs, and identifiers when known.
- Do not mention the summary process or that context was compacted."#;

fn truncate(s: &str) -> String {
    if s.chars().count() <= TOOL_OUTPUT_MAX_CHARS {
        s.to_string()
    } else {
        let cut: String = s.chars().take(TOOL_OUTPUT_MAX_CHARS).collect();
        format!("{cut}\n[truncated]")
    }
}

fn attach_line(item: &Value) -> String {
    let mime = item["mime"].as_str().unwrap_or("file");
    let name = item["filename"].as_str().unwrap_or("file");
    format!("[Attached {mime}: {name}]")
}

/// v1 `serialize` (session/compaction.ts:54-95): `[User]:`/`[Assistant]:`
/// lines, tool call + result lines, reasoning line, attachments, output
/// truncation at 2000 chars, compacted results → cleared marker.
pub fn serialize(info: &Value, parts: &[Value]) -> String {
    let role = info["role"].as_str().unwrap_or("user");
    if role == "user" {
        let text: String = parts
            .iter()
            .filter(|p| p["type"] == "text" && p["ignored"] != true)
            .filter_map(|p| p["text"].as_str())
            .filter(|t| !t.is_empty())
            .collect::<Vec<_>>()
            .join("\n");
        let files: Vec<String> = parts
            .iter()
            .filter(|p| p["type"] == "file")
            .map(attach_line)
            .collect();
        let mut lines = Vec::new();
        if !text.is_empty() {
            lines.push(format!("[User]: {text}"));
        }
        lines.extend(files);
        return lines.join("\n");
    }
    let mut lines: Vec<String> = Vec::new();
    for p in parts {
        match p["type"].as_str().unwrap_or("") {
            "text" => {
                let t = p["text"].as_str().unwrap_or("");
                if !t.is_empty() {
                    lines.push(format!("[Assistant]: {t}"));
                }
            }
            "reasoning" => {
                let t = p["text"].as_str().unwrap_or("");
                if !t.is_empty() {
                    lines.push(format!("[Assistant reasoning]: {t}"));
                }
            }
            "tool" => {
                let tool = p["tool"].as_str().unwrap_or("tool");
                let call = format!("[Assistant tool call]: {tool}({})", p["state"]["input"]);
                match p["state"]["status"].as_str().unwrap_or("") {
                    "completed" => {
                        let compacted = p["state"]["time"]["compacted"].as_bool().unwrap_or(false);
                        let attachments: Vec<String> = p["state"]["attachments"]
                            .as_array()
                            .map(|a| a.iter().map(attach_line).collect())
                            .unwrap_or_default();
                        let output = if compacted {
                            "[Old tool result content cleared]".to_string()
                        } else {
                            let outs: Vec<&str> =
                                std::iter::once(p["state"]["output"].as_str().unwrap_or(""))
                                    .chain(attachments.iter().map(String::as_str))
                                    .collect();
                            truncate(&outs.join("\n"))
                        };
                        lines.push(call);
                        lines.push(format!("[Tool result]: {output}"));
                    }
                    "error" => {
                        lines.push(call);
                        lines.push(format!(
                            "[Tool error]: {}",
                            p["state"]["error"].as_str().unwrap_or("")
                        ));
                    }
                    _ => lines.push(call),
                }
            }
            _ => {}
        }
    }
    lines.join("\n")
}

/// v1 `buildPrompt` first-time path (core compaction.ts:160-167).
pub fn build_summary_prompt(entries: &[String]) -> String {
    let conversation = format!(
        "Here is the conversation so far:\n\n<conversation>\n{}\n</conversation>",
        entries.join("\n\n")
    );
    format!("{conversation}\n\n{INSTRUCTION}\n\n{SUMMARY_TEMPLATE}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn serialize_user_assistant_tool_rules() {
        let user = json!({"role": "user"});
        let parts = vec![
            json!({"type":"text","text":"do the thing"}),
            json!({"type":"file","mime":"image/png","filename":"a.png"}),
        ];
        assert_eq!(
            serialize(&user, &parts),
            "[User]: do the thing\n[Attached image/png: a.png]"
        );
        let asst = json!({"role": "assistant"});
        let tparts = vec![
            json!({"type":"text","text":"running it"}),
            json!({"type":"tool","tool":"bash",
                "state":{"status":"completed","input":{"command":"ls"},
                    "output":"file1\nfile2","time":{}}}),
        ];
        let s = serialize(&asst, &tparts);
        assert!(s.contains("[Assistant]: running it"));
        assert!(s.contains("[Assistant tool call]: bash({\"command\":\"ls\"})"));
        assert!(s.contains("[Tool result]: file1\nfile2"));
        // truncation at 2000 chars
        let long = "x".repeat(3000);
        let lparts = vec![json!({"type":"tool","tool":"bash",
            "state":{"status":"completed","input":{},"output":long,"time":{}}})];
        let s = serialize(&asst, &lparts);
        assert!(s.ends_with("\n[truncated]"), "truncation marker");
        assert!(s.len() < 2200, "bounded output: {}", s.len());
        // error form
        let eparts = vec![json!({"type":"tool","tool":"bash",
            "state":{"status":"error","input":{},"error":"boom"}})];
        assert!(serialize(&asst, &eparts).contains("[Tool error]: boom"));
    }

    #[test]
    fn summary_prompt_shape() {
        let p = build_summary_prompt(&["[User]: hello".into(), "[Assistant]: hi".into()]);
        assert!(p.starts_with("Here is the conversation so far:"));
        assert!(p.contains("<conversation>\n[User]: hello\n\n[Assistant]: hi\n</conversation>"));
        assert!(p.contains("Create a new anchored summary"));
        assert!(p.contains("## Objective"));
        assert!(p.contains("## Relevant Files"));
        assert!(p.contains("Do not mention the summary process"));
    }
}
