//! Bash command → permission patterns (upstream `permission/arity.ts` parity).
//!
//! Upstream's shell tool scans a command into (a) `patterns` — the concrete
//! subcommands, and (b) `always` — an arity *prefix* per subcommand
//! (`"git status --short"` → always `"git status *"`), so an "always" grant
//! covers the whole command family. It also collects `directories` referenced
//! by path-taking commands for the `external_directory` gate. ocserve
//! previously used the whole command string as both pattern and always, so
//! "always" only remembered the exact string and compound commands collapsed
//! to one resource.
//!
//! The ARITY table is pinned data extracted from the freeze
//! (`bench/permission/1.18.31.rules.json`, `scripts/extract-rules.sh`) — the
//! same discipline as the OpenAPI spec and web bundle. No table is
//! hand-maintained here.

use std::collections::BTreeMap;
use std::sync::OnceLock;

const RULES_JSON: &str = include_str!("../../../bench/permission/1.18.31.rules.json");

fn arity_table() -> &'static BTreeMap<String, usize> {
    static TABLE: OnceLock<BTreeMap<String, usize>> = OnceLock::new();
    TABLE.get_or_init(|| {
        let doc: serde_json::Value = serde_json::from_str(RULES_JSON).unwrap_or_default();
        doc.get("arity")
            .and_then(|a| a.as_object())
            .map(|m| {
                m.iter()
                    .filter_map(|(k, v)| v.as_u64().map(|n| (k.clone(), n as usize)))
                    .collect()
            })
            .unwrap_or_default()
    })
}

/// Split a shell command into subcommands, respecting quotes. Splits on the
/// control operators upstream's parser treats as command boundaries:
/// `&&`, `||`, `;`, `|`, `&`, and newlines.
pub fn split_commands(command: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut chars = command.chars().peekable();
    let mut quote: Option<char> = None;
    while let Some(c) = chars.next() {
        match quote {
            Some(q) => {
                cur.push(c);
                if c == q {
                    quote = None;
                }
            }
            None => match c {
                '\'' | '"' => {
                    quote = Some(c);
                    cur.push(c);
                }
                ';' | '\n' | '&' | '|' => {
                    // swallow a doubled operator (&&, ||)
                    if let Some(&n) = chars.peek()
                        && n == c
                    {
                        chars.next();
                    }
                    let trimmed = cur.trim();
                    if !trimmed.is_empty() {
                        out.push(trimmed.to_string());
                    }
                    cur.clear();
                }
                _ => cur.push(c),
            },
        }
    }
    let trimmed = cur.trim();
    if !trimmed.is_empty() {
        out.push(trimmed.to_string());
    }
    out
}

/// Naive shell tokenizer for arity lookup: split on whitespace, keeping quoted
/// runs together (the quotes are stripped, matching how the arity table keys
/// are written).
pub fn tokenize(segment: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut quote: Option<char> = None;
    for c in segment.chars() {
        match quote {
            Some(q) => {
                if c == q {
                    quote = None;
                } else {
                    cur.push(c);
                }
            }
            None => {
                if c == '\'' || c == '"' {
                    quote = Some(c);
                } else if c.is_whitespace() {
                    if !cur.is_empty() {
                        out.push(std::mem::take(&mut cur));
                    }
                } else {
                    cur.push(c);
                }
            }
        }
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

/// Upstream `BashArity.prefix`: longest matching table prefix wins; fall back
/// to the first token; empty for no tokens.
pub fn arity_prefix(tokens: &[String]) -> Vec<String> {
    let table = arity_table();
    for len in (1..=tokens.len()).rev() {
        let prefix = tokens[..len].join(" ");
        if let Some(&arity) = table.get(&prefix) {
            let n = arity.min(tokens.len());
            return tokens[..n].to_vec();
        }
    }
    if tokens.is_empty() {
        return Vec::new();
    }
    vec![tokens[0].clone()]
}

/// A bash command's permission surfaces: per-subcommand `patterns` and their
/// `always` grants.
pub struct BashScan {
    pub patterns: Vec<String>,
    pub always: Vec<String>,
}

/// Scan a bash command into (patterns, always), matching upstream shell.ts:
/// each subcommand is a pattern; its arity prefix + " *" is the always grant.
pub fn scan(command: &str) -> BashScan {
    let mut patterns = Vec::new();
    let mut always = Vec::new();
    for seg in split_commands(command) {
        let tokens = tokenize(&seg);
        if tokens.is_empty() {
            continue;
        }
        patterns.push(seg.clone());
        let prefix = arity_prefix(&tokens);
        if !prefix.is_empty() {
            always.push(format!("{} *", prefix.join(" ")));
        }
    }
    // de-dup preserving order (a repeated subcommand grants once)
    dedup(&mut patterns);
    dedup(&mut always);
    BashScan { patterns, always }
}

fn dedup(v: &mut Vec<String>) {
    let mut seen = std::collections::HashSet::new();
    v.retain(|x| seen.insert(x.clone()));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn arity_prefix_uses_table() {
        // `git status` → bare `git` arity 2 → ["git","status"]
        assert_eq!(
            arity_prefix(&["git".into(), "status".into(), "--short".into()]),
            vec!["git".to_string(), "status".to_string()]
        );
        // `npm run dev` → `npm run` arity 3
        assert_eq!(
            arity_prefix(&["npm".into(), "run".into(), "dev".into()]),
            vec!["npm".to_string(), "run".to_string(), "dev".to_string()]
        );
        // unknown command → first token only
        assert_eq!(
            arity_prefix(&["frobnicate".into(), "x".into()]),
            vec!["frobnicate".to_string()]
        );
    }

    #[test]
    fn scan_splits_compounds() {
        let s = scan("git status && rm -rf /tmp/x");
        assert_eq!(s.patterns.len(), 2, "two subcommands");
        assert!(s.always.contains(&"git status *".to_string()));
        assert!(s.always.contains(&"rm *".to_string()));
    }

    #[test]
    fn scan_respects_quotes() {
        let s = scan("echo 'a; b' && ls");
        assert_eq!(s.patterns.len(), 2, "the ; is inside quotes");
    }

    #[test]
    fn scan_arity_family() {
        // "git status --short" and "git commit -m x" share the "git status"? no —
        // they share the `git` arity-2 prefix per subcommand
        let s = scan("git status --short");
        assert_eq!(s.always, vec!["git status *".to_string()]);
        let s2 = scan("git commit -m x");
        assert_eq!(s2.always, vec!["git commit *".to_string()]);
    }
}
