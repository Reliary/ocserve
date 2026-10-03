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
                        // JS truthy: upstream checks `part.state.time.compacted` —
                        // a NUMBER timestamp is truthy (as_bool() missed it = BUG
                        // caught by M6 reading; prune marks never rendered)
                        let ct = &p["state"]["time"]["compacted"];
                        let compacted = match ct {
                            Value::Null => false,
                            Value::Bool(b) => *b,
                            Value::Number(n) => n.as_f64() != Some(0.0),
                            Value::String(t) => !t.is_empty(),
                            _ => true, // {} / [] are JS-truthy
                        };
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

// ─────────────────────────── M6: retention + selection ───────────────────────────
// Ports of message-v2.ts filterCompacted (518-578, iterates newest-first per
// orderBy desc at :443) + compaction.ts select/turns/splitTurn/preserve-budget
// (COMPACTION.md §2.5/§2.6). Indices are into the caller's message list —
// never a second full copy (COMPACTION §5 P1: projection/indices, no O(N) clone).

use std::collections::{HashMap, HashSet};

/// Config keys (shared opencode.json — config.ts:149-165 read-sites):
/// auto (default true), prune (default false), tail_turns,
/// preserve_recent_tokens, reserved.
#[derive(Debug, Clone, PartialEq)]
pub struct CompactionCfg {
    pub auto: bool,
    pub prune: bool,
    pub tail_turns: Option<i64>,
    pub preserve_recent_tokens: Option<i64>,
    pub reserved: Option<i64>,
}

impl Default for CompactionCfg {
    fn default() -> Self {
        Self {
            auto: true,
            prune: false,
            tail_turns: None,
            preserve_recent_tokens: None,
            reserved: None,
        }
    }
}

impl CompactionCfg {
    pub fn from_value(v: Option<&Value>) -> Self {
        let mut c = Self::default();
        let Some(v) = v else { return c };
        if let Some(b) = v["auto"].as_bool() {
            c.auto = b;
        }
        if let Some(b) = v["prune"].as_bool() {
            c.prune = b;
        }
        c.tail_turns = v["tail_turns"].as_i64();
        c.preserve_recent_tokens = v["preserve_recent_tokens"].as_i64();
        c.reserved = v["reserved"].as_i64();
        c
    }
}

const COMPACTION_BUFFER: i64 = 20_000;
const MIN_PRESERVE_RECENT_TOKENS: i64 = 2_000;
const MAX_PRESERVE_RECENT_TOKENS: i64 = 15_000;

/// v1 overflow.ts usable(): input−reserved when limit.input exists, else
/// context − maxOutput; reserved = cfg.reserved ?? min(20k, maxOutput).
/// context <= 0 → 0 (auto-compaction disabled — fail OFF, COMPACTION D4).
pub fn usable(cfg: &CompactionCfg, limit_input: i64, limit_context: i64, max_output: i64) -> i64 {
    if limit_context <= 0 && limit_input <= 0 {
        return 0;
    }
    let reserved = cfg
        .reserved
        .unwrap_or_else(|| COMPACTION_BUFFER.min(max_output.max(0)));
    if limit_input > 0 {
        (limit_input - reserved).max(0)
    } else {
        (limit_context - max_output.max(0)).max(0)
    }
}

/// overflow.ts isOverflow — `count` = total when present else components.
pub fn is_overflow(count_total: i64, usable_tokens: i64) -> bool {
    usable_tokens > 0 && count_total >= usable_tokens
}

/// compaction.ts:116-119 preserveRecentBudget.
pub fn preserve_recent_budget(cfg: &CompactionCfg, usable_tokens: i64) -> i64 {
    cfg.preserve_recent_tokens.unwrap_or_else(|| {
        MAX_PRESERVE_RECENT_TOKENS
            .min(MIN_PRESERVE_RECENT_TOKENS.max((usable_tokens as f64 * 0.25) as i64))
    })
}

fn has_compaction_part(parts: &[Value]) -> bool {
    parts.iter().any(|p| p["type"] == "compaction")
}

fn compaction_tail(parts: &[Value]) -> Option<String> {
    parts
        .iter()
        .find(|p| p["type"] == "compaction")
        .and_then(|p| p["tail_start_id"].as_str())
        .map(str::to_string)
}

fn is_completed_summary(info: &Value) -> bool {
    info["role"] == "assistant"
        && info["summary"] == Value::Bool(true)
        && info["finish"].is_string()
        && info["error"].is_null()
}

/// v1 filterCompacted — returns selected indices into `messages` (input is
/// ASC = chronological; upstream walks DESC then reverses — equivalent).
pub fn filter_compacted(messages: &[(Value, Vec<Value>)]) -> Vec<usize> {
    let n = messages.len();
    let mut completed: HashSet<String> = HashSet::new();
    let mut retain: Option<String> = None;
    // DESC walk producing chronological selection via a reversed scratch list
    let mut sel: Vec<usize> = Vec::with_capacity(n);
    for i in (0..n).rev() {
        let (info, parts) = &messages[i];
        sel.push(i);
        if let Some(ret) = &retain {
            if info["id"].as_str() == Some(ret.as_str()) {
                break;
            }
            continue;
        }
        if info["role"] == "user" && completed.contains(info["id"].as_str().unwrap_or("")) {
            let Some(tail) = compaction_tail(parts) else {
                break; // anchor without tail: keep through the anchor, drop older
            };
            retain = Some(tail.clone());
            if info["id"].as_str() == Some(tail.as_str()) {
                break;
            }
            continue;
        }
        if is_completed_summary(info)
            && let Some(parent) = info["parentID"].as_str()
        {
            completed.insert(parent.to_string());
        }
    }
    sel.reverse(); // chronological

    // Reassembly (message-v2.ts:553-576): last tail-carrying anchor + its
    // summary pulled forward, verbatim tail behind them, recent last.
    let compaction_index = sel.iter().rposition(|&i| {
        messages[i].0["role"] == "user" && compaction_tail(&messages[i].1).is_some()
    });
    if let Some(ci) = compaction_index {
        let anchor_id = messages[sel[ci]].0["id"].as_str().unwrap_or("").to_string();
        let summary_index = sel.iter().position(|&i| {
            let info = &messages[i].0;
            info["role"] == "assistant"
                && info["summary"] == Value::Bool(true)
                && info["parentID"].as_str() == Some(anchor_id.as_str())
        });
        if let Some(si) = summary_index
            && si > ci
        {
            let tail_id = compaction_tail(&messages[sel[ci]].1).unwrap_or_default();
            let tail_index = sel
                .iter()
                .position(|&i| messages[i].0["id"].as_str() == Some(tail_id.as_str()));
            if let Some(ti) = tail_index
                && ti < ci
            {
                let mut out = Vec::with_capacity(sel.len());
                out.extend(&sel[ci..=si]); // anchor + summary
                out.extend(&sel[ti..ci]); // verbatim tail
                out.extend(&sel[si + 1..]); // everything after summary
                return out;
            }
        }
    }
    sel
}

/// A completed compaction pair resolved against the loaded messages:
/// (anchor index, summary index, summary text). Uses the projection rows
/// (COMPACTION §4) — no message scan for discovery; missing messages in a
/// partial load are skipped honestly.
pub fn completed_compactions(
    messages: &[(Value, Vec<Value>)],
    rows: &[refine_store::CompactionRow],
) -> Vec<(usize, usize, Option<String>)> {
    let by_id: HashMap<&str, usize> = messages
        .iter()
        .enumerate()
        .filter_map(|(i, (info, _))| info["id"].as_str().map(|id| (id, i)))
        .collect();
    let mut out = Vec::new();
    for row in rows {
        let (Some(&ui), Some(&si)) = (
            by_id.get(row.user_msg_id.as_str()),
            by_id.get(row.summary_msg_id.as_deref().unwrap_or("")),
        ) else {
            continue;
        };
        let info = &messages[si].0;
        if !is_completed_summary(info)
            || info["parentID"].as_str() != Some(row.user_msg_id.as_str())
        {
            continue;
        }
        out.push((ui, si, summary_text(&messages[si].1)));
    }
    out
}

/// compaction.ts summaryText — non-empty text parts joined by blank line.
pub fn summary_text(parts: &[Value]) -> Option<String> {
    let t: Vec<&str> = parts
        .iter()
        .filter(|p| p["type"] == "text")
        .filter_map(|p| p["text"].as_str())
        .map(str::trim)
        .filter(|t| !t.is_empty())
        .collect();
    if t.is_empty() {
        None
    } else {
        Some(t.join("\n\n"))
    }
}

/// Turn boundaries over a (hidden-filtered) list: a turn starts at every
/// user message WITHOUT a compaction part (compaction.ts turns(), :131-143).
fn turns(messages: &[(Value, Vec<Value>)]) -> Vec<(usize, usize, Option<String>)> {
    let mut starts: Vec<(usize, Option<String>)> = Vec::new();
    for (i, (info, parts)) in messages.iter().enumerate() {
        if info["role"] == "user" && !has_compaction_part(parts) {
            starts.push((i, info["id"].as_str().map(str::to_string)));
        }
    }
    let mut out = Vec::with_capacity(starts.len());
    for (k, (start, id)) in starts.iter().enumerate() {
        let end = starts.get(k + 1).map(|(n, _)| *n).unwrap_or(messages.len());
        out.push((*start, end, id.clone()));
    }
    out
}

/// Token estimator for select(): maps a message slice to a count.
pub type EstimateFn = dyn Fn(&[(Value, Vec<Value>)]) -> i64;

/// v1 select (compaction.ts:218-266): recent-turn tail bounded by
/// preserve_recent_tokens budget (+ tail_turns cap). `estimate` maps a
/// message slice to a token count — production passes the
/// to_provider_messages-JSON estimator (COMPACTION §5 P8/estimate parity:
/// Token.estimate = round(len/4), core/util/token.ts:5).
pub fn select(
    messages: &[(Value, Vec<Value>)],
    cfg: &CompactionCfg,
    usable_tokens: i64,
    estimate: &EstimateFn,
) -> (Vec<usize>, Option<String>) {
    if let Some(limit) = cfg.tail_turns
        && limit <= 0
    {
        return ((0..messages.len()).collect(), None);
    }
    let budget = preserve_recent_budget(cfg, usable_tokens);
    let all = turns(messages);
    if all.is_empty() {
        return ((0..messages.len()).collect(), None);
    }
    let recent: &[(usize, usize, Option<String>)] = match cfg.tail_turns {
        Some(limit) if (limit as usize) < all.len() => &all[all.len() - limit as usize..],
        _ => &all,
    };
    let mut total = 0i64;
    let mut keep: Option<(usize, Option<String>)> = None;
    for (start, end, id) in recent.iter().rev() {
        let size = estimate(&messages[*start..*end]);
        if total + size <= budget {
            total += size;
            keep = Some((*start, id.clone()));
            continue;
        }
        let remaining = budget - total;
        // splitTurn: first suffix of this turn that fits the remainder
        if remaining > 0 {
            for st in (*start + 1)..*end {
                if estimate(&messages[st..*end]) <= remaining {
                    keep = Some((st, messages[st].0["id"].as_str().map(str::to_string)));
                    break;
                }
            }
        }
        break;
    }
    let Some((keep_start, keep_id)) = keep else {
        return ((0..messages.len()).collect(), None);
    };
    if keep_start == 0 {
        return ((0..messages.len()).collect(), None);
    }
    ((0..keep_start).collect(), keep_id)
}

/// v1 buildPrompt update path (core compaction.ts:168-177).
pub fn build_summary_update_prompt(previous_summary: &str, entries: &[String]) -> String {
    let conversation = format!(
        "Here is the conversation so far:\n\n<conversation>\n{}\n</conversation>",
        entries.join("\n\n")
    );
    format!(
        "{conversation}\n\nHere is the summary of the conversation before the <conversation> above:\n\n<prior-summary>\n{previous_summary}\n</prior-summary>\n\n{SUMMARY_UPDATE_INSTRUCTIONS}\n\n{SUMMARY_TEMPLATE}"
    )
}

/// SUMMARY_UPDATE_INSTRUCTIONS verbatim (core compaction.ts:47-60).
pub const SUMMARY_UPDATE_INSTRUCTIONS: &str = r#"The <prior-summary> summarizes everything that happened before the <conversation>. Construct a new summary that combines both. The <prior-summary> is discarded after this: anything you do not carry into the new summary is lost.

When combining:
- Carry forward objectives, constraints, user directives, decisions, and parallel workstreams from the <prior-summary> even when the <conversation> does not mention them. Drop only what is finished and no longer needed.
- The <conversation> is more recent than the <prior-summary>. Where they conflict, the conversation wins: state the corrected fact and drop the old claim.
- Add new progress, decisions, constraints, and context from the conversation.
- Move completed work from "Active" to "Completed".
- If a blocker has been resolved, update the summary to reflect that while keeping any details still needed to continue the work.
- Update "Objective" and "Next Move" to reflect the current work state."#;

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

#[cfg(test)]
mod m6_tests {
    use super::*;
    use serde_json::json;

    fn user(id: &str) -> Value {
        json!({"id": id, "role": "user", "time": {"created": 1}})
    }
    fn asst(id: &str) -> Value {
        json!({"id": id, "role": "assistant", "finish": "stop", "time": {"created": 2}})
    }
    fn summary_asst(id: &str, parent: &str) -> Value {
        json!({"id": id, "role": "assistant", "summary": true, "finish": "stop",
               "parentID": parent, "time": {"created": 2}})
    }
    fn text(id: &str, t: &str) -> Value {
        json!({"id": id, "type": "text", "text": t})
    }
    fn anchor(id: &str, tail: Option<&str>) -> Vec<Value> {
        let mut p = json!({"id": format!("prt_{id}"), "type": "compaction", "auto": true});
        if let Some(t) = tail {
            p["tail_start_id"] = json!(t);
        }
        vec![p]
    }

    fn ids<'a>(messages: &'a [(Value, Vec<Value>)], sel: &[usize]) -> Vec<&'a str> {
        sel.iter()
            .map(|&i| messages[i].0["id"].as_str().unwrap())
            .collect()
    }

    #[test]
    fn filter_identity_without_anchor() {
        let msgs: Vec<(Value, Vec<Value>)> = vec![
            (user("m1"), vec![text("p1", "hi")]),
            (asst("m2"), vec![text("p2", "yo")]),
        ];
        assert_eq!(filter_compacted(&msgs), vec![0, 1]);
    }

    #[test]
    fn filter_anchor_without_tail_drops_older_once_completed() {
        // pending anchor (no completed summary yet) keeps FULL history —
        // upstream filter only tightens once a summary assistant exists
        let pending: Vec<(Value, Vec<Value>)> = vec![
            (user("old"), vec![text("po", "ancient")]),
            (user("anch"), anchor("anch", None)),
            (asst("s1"), vec![text("ps1", "answer")]),
        ];
        assert_eq!(
            filter_compacted(&pending).len(),
            3,
            "pending state: no drop"
        );

        // completed anchor WITHOUT tail: keep through anchor, drop older
        let msgs: Vec<(Value, Vec<Value>)> = vec![
            (user("old"), vec![text("po", "ancient")]),
            (user("anch"), anchor("anch", None)),
            (
                summary_asst("s1", "anch"),
                vec![text("ps1", "summary of old")],
            ),
            (user("new"), vec![text("pn", "recent")]),
        ];
        let sel = filter_compacted(&msgs);
        assert_eq!(ids(&msgs, &sel), vec!["anch", "s1", "new"]);
    }

    #[test]
    fn filter_full_reassembly_order() {
        // anchor carries tail=msg_t (older, verbatim bridge)
        let msgs: Vec<(Value, Vec<Value>)> = vec![
            (user("ancient"), vec![text("pa", "drop me")]),
            (user("msg_t"), vec![text("pt", "tail turn")]),
            (user("mid"), vec![text("pm", "between tail and anchor")]),
            (user("anch"), anchor("anch", Some("msg_t"))),
            (
                summary_asst("summ", "anch"),
                vec![text("psum", "the summary")],
            ),
            (user("newer"), vec![text("pnew", "after compaction")]),
            (asst("end"), vec![text("pend", "answer")]),
        ];
        let sel = filter_compacted(&msgs);
        // DESC walk stops at tail (ancient never pushed); reassembly orders
        // anchor+summary, then verbatim tail..anchor, then post-summary.
        assert_eq!(
            ids(&msgs, &sel),
            vec!["anch", "summ", "msg_t", "mid", "newer", "end"]
        );
    }

    #[test]
    fn filter_missing_summary_skips_reassembly() {
        let msgs: Vec<(Value, Vec<Value>)> = vec![
            (user("msg_t"), vec![text("pt", "t")]),
            (user("mid"), vec![text("pm", "m")]),
            (user("anch"), anchor("anch", Some("msg_t"))),
            (user("newer"), vec![text("pn", "n")]),
        ];
        let sel = filter_compacted(&msgs);
        // no summary assistant → chronological selection unchanged
        assert_eq!(ids(&msgs, &sel), vec!["msg_t", "mid", "anch", "newer"]);
    }

    #[test]
    fn select_respects_budget_and_tail_turns() {
        let msgs: Vec<(Value, Vec<Value>)> = vec![
            (user("u1"), vec![text("p1", "aaaa")]), // turn 1
            (asst("a1"), vec![text("p2", "bbbb")]),
            (user("u2"), vec![text("p3", "cccc")]), // turn 2
            (asst("a2"), vec![text("p4", "dddd")]),
            (user("u3"), vec![text("p5", "eeee")]), // turn 3
            (asst("a3"), vec![text("p6", "ffff")]),
        ];
        // estimator: 1 token per message (constant)
        let est = |slice: &[(Value, Vec<Value>)]| slice.len() as i64;
        let cfg = CompactionCfg {
            tail_turns: Some(1),
            preserve_recent_tokens: Some(2),
            ..Default::default()
        };
        let (head, tail) = select(&msgs, &cfg, 100_000, &est);
        // only the last turn (u3,a3) fits the budget → head = before u3
        assert_eq!(ids(&msgs, &head), vec!["u1", "a1", "u2", "a2"]);
        assert_eq!(tail.as_deref(), Some("u3"));

        // tail_turns <= 0 keeps everything (compaction.ts:219-221)
        let cfg0 = CompactionCfg {
            tail_turns: Some(0),
            ..Default::default()
        };
        let (head0, tail0) = select(&msgs, &cfg0, 100_000, &est);
        assert_eq!(head0.len(), 6);
        assert_eq!(tail0, None);
    }

    #[test]
    fn usable_and_overflow_math() {
        let cfg = CompactionCfg::default();
        // context path: 1000 context − max_output 100 = 900 usable
        assert_eq!(usable(&cfg, 0, 1000, 100), 900);
        // input path wins when present: input 5000 − min(20k, out=8192) = 5000-8192<0 → 0
        assert_eq!(usable(&cfg, 5000, 1000, 8192), 0);
        // reserved applies on the limit.input path ONLY (overflow.ts:27-31:
        // context path = context − maxOutput, no reserved)
        let cfg_r = CompactionCfg {
            reserved: Some(50),
            ..Default::default()
        };
        assert_eq!(usable(&cfg_r, 0, 1000, 100), 900);
        assert_eq!(usable(&cfg_r, 1000, 1000, 100), 950);
        // fail OFF on missing context
        assert_eq!(usable(&cfg, 0, 0, 0), 0);
        assert!(!is_overflow(899, 900), "below usable is not overflow");
        assert!(is_overflow(900, 900));
        assert!(!is_overflow(1_000_000, 0), "usable=0 disables overflow");
        // preserve budget defaults
        assert_eq!(preserve_recent_budget(&cfg, 100_000), 15_000);
        assert_eq!(preserve_recent_budget(&cfg, 4_000), 2_000);
        assert_eq!(preserve_recent_budget(&cfg, 100), 2_000);
    }

    #[test]
    fn update_prompt_shape_matches_upstream() {
        let p = build_summary_update_prompt("prior summary text", &["[User]: new stuff".into()]);
        assert!(p.starts_with("Here is the conversation so far:"));
        assert!(p.contains("<prior-summary>\nprior summary text\n</prior-summary>"));
        assert!(p.contains("conversation wins"));
        assert!(p.contains("## Next Move"));
        assert!(p.contains("Do not mention the summary process"));
    }

    #[test]
    fn serialize_renders_numeric_compacted_marker() {
        // regression: as_bool() missed NUMBER timestamps (JS truthy = any
        // number) — prune marks must render as cleared
        let asst_info = json!({"role": "assistant"});
        let parts = vec![json!({"type":"tool","tool":"bash",
            "state":{"status":"completed","input":{},"output":"SECRET",
                     "time":{"compacted":1791000000000u64}}})];
        let s = serialize(&asst_info, &parts);
        assert!(s.contains("[Old tool result content cleared]"), "got: {s}");
        assert!(!s.contains("SECRET"), "pruned output must not leak: {s}");
    }

    #[test]
    fn completed_compactions_via_projection() {
        let msgs: Vec<(Value, Vec<Value>)> = vec![
            (user("msg_t"), vec![text("pt", "t")]),
            (user("anch"), anchor("anch", Some("msg_t"))),
            (
                summary_asst("summ", "anch"),
                vec![text("psum", "the summary")],
            ),
            (user("newer"), vec![text("pn", "n")]),
        ];
        let rows = vec![refine_store::CompactionRow {
            part_id: "prt_anch".into(),
            user_msg_id: "anch".into(),
            auto: true,
            overflow: false,
            tail_start_id: Some("msg_t".into()),
            summary_msg_id: Some("summ".into()),
        }];
        let pairs = completed_compactions(&msgs, &rows);
        assert_eq!(pairs.len(), 1);
        assert_eq!((pairs[0].0, pairs[0].1), (1, 2));
        assert_eq!(pairs[0].2.as_deref(), Some("the summary"));
        // summary text empty → None (upstream summaryText)
        assert_eq!(summary_text(&[]), None);
    }
}
