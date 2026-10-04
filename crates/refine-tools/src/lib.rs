//! refine-tools: v1 tool names + permission evaluation (PLAN §11: v1 names
//! kept; no v2 renames). Wildcard/evaluate ported from v1 util/wildcard.ts
//! and core/permission.ts; tool params verified against v1 sources + live
//! tool-part capture (testdata/m2).

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};

/// v1 Wildcard.match: glob → regex, `\`→`/`, trailing " *" tail optional.
pub fn wildcard_match(s: &str, pattern: &str) -> bool {
    let s = s.replace('\\', "/");
    let pattern = pattern.replace('\\', "/");
    let mut escaped = String::with_capacity(pattern.len() * 2);
    for c in pattern.chars() {
        match c {
            '.' | '+' | '^' | '$' | '{' | '}' | '(' | ')' | '|' | '[' | ']' | '\\' => {
                escaped.push('\\');
                escaped.push(c);
            }
            '*' => escaped.push_str(".*"),
            '?' => escaped.push('.'),
            other => escaped.push(other),
        }
    }
    // v1 quirk: trailing " *" (space+wildcard) → optional tail ("ls *" ≈ "ls")
    if escaped.ends_with(" .*") {
        escaped.truncate(escaped.len() - 3);
        escaped.push_str("( .*)?");
    }
    let re = format!("^{escaped}$");
    glob_to_regex_match(&s, &re)
}

/// Minimal glob-regex matcher (no regex crate — patterns are simple globs
/// already converted above). Compiles the escaped pattern manually.
fn glob_to_regex_match(s: &str, re_like: &str) -> bool {
    // re_like is ^...$ with .* / . / \( .*)? and escaped literals.
    regex_match(s.as_bytes(), re_like.as_bytes())
}

/// Tiny backtracking matcher supporting: ^ $ . .* . and ( .*)? group.
fn regex_match(s: &[u8], p: &[u8]) -> bool {
    fn helper(s: &[u8], p: &[u8]) -> bool {
        if p.is_empty() {
            return s.is_empty();
        }
        match p[0] {
            b'^' => helper(s, &p[1..]),
            b'$' => p.len() == 1 && s.is_empty(),
            // ".*" (dot THEN star): any-length run with backtracking
            b'.' if p.len() > 1 && p[1] == b'*' => {
                for i in 0..=s.len() {
                    if helper(&s[i..], &p[2..]) {
                        return true;
                    }
                }
                false
            }
            b'.' => !s.is_empty() && helper(&s[1..], &p[1..]),
            b'(' if p.starts_with(b"( .*)?") => {
                // optional " .*" group: either consume up to next ')' match or skip
                let rest = &p[b"( .*)?".len()..];
                // skip: group matches empty
                if helper(s, rest) {
                    return true;
                }
                // consume: any prefix (group = " .*" ≈ " <anything>")
                if s.is_empty() {
                    return false;
                }
                // must include at least the space per " .*"
                if s[0] != b' ' {
                    return false;
                }
                for i in 1..=s.len() {
                    if helper(&s[i..], rest) {
                        return true;
                    }
                }
                false
            }
            b'\\' if p.len() > 1 => !s.is_empty() && s[0] == p[1] && helper(&s[1..], &p[2..]),
            lit => !s.is_empty() && s[0] == lit && helper(&s[1..], &p[1..]),
        }
    }
    // strip trailing lone $ handled in helper; strip leading ^ once
    helper(s, p)
}

/// v1 permission evaluation: find LAST rule matching (permission, resource);
/// default = ask (core/permission.ts evaluate()).
#[derive(Clone, Debug)]
pub struct Rule {
    pub permission: String,
    pub pattern: String,
    pub action: String,
}

pub fn evaluate(permission: &str, resource: &str, rules: &[Rule]) -> String {
    rules
        .iter()
        .rev()
        .find(|r| wildcard_match(permission, &r.permission) && wildcard_match(resource, &r.pattern))
        .map(|r| r.action.clone())
        .unwrap_or_else(|| "ask".to_string())
}

/// Truncation (v1 shell: MAX_LINES/MAX_BYTES constants).
pub const MAX_LINES: usize = 2000;
pub const MAX_BYTES: usize = 50 * 1024;

pub struct ToolResult {
    pub output: String,
    pub truncated: bool,
    pub exit: Option<i32>,
    pub title: String,
    pub error: bool,
    /// Custom part-state metadata (todowrite → {todos}); None → bash-style
    /// {output, exit, truncated} template in the runner.
    pub metadata: Option<serde_json::Value>,
}

/// todowrite: validate the list (persistence happens in the runner, which
/// owns the writer + event bus; v1 todowrite also runs permission.assert —
/// our loop-level gate covers that before execute).
fn todowrite(input: &Value) -> Result<ToolResult> {
    let todos = input
        .get("todos")
        .and_then(|t| t.as_array())
        .ok_or_else(|| anyhow::anyhow!("todowrite.todos must be an array"))?;
    let mut normalized = Vec::with_capacity(todos.len());
    for t in todos {
        let content = t
            .get("content")
            .and_then(|c| c.as_str())
            .ok_or_else(|| anyhow::anyhow!("todo.content required"))?;
        let status = t
            .get("status")
            .and_then(|x| x.as_str())
            .unwrap_or("pending");
        let priority = t.get("priority").and_then(|x| x.as_str()).unwrap_or("");
        normalized.push(json!({
            "content": content,
            "status": status,
            "priority": priority,
        }));
    }
    let output = serde_json::to_string_pretty(&json!({"todos": normalized}))?;
    Ok(ToolResult {
        output,
        truncated: false,
        exit: None,
        title: "todowrite".into(),
        error: false,
        metadata: Some(json!({"todos": normalized})),
    })
}

pub fn schemas() -> Vec<Value> {
    vec![
        json!({"type":"function","function":{"name":"bash","description":"Run a shell command and return its output.","parameters":{"type":"object","properties":{"command":{"type":"string","description":"The command to execute"},"timeout":{"type":"number","description":"Optional timeout in milliseconds"}},"required":["command"]}}}),
        json!({"type":"function","function":{"name":"read","description":"Read a file from the filesystem.","parameters":{"type":"object","properties":{"filePath":{"type":"string","description":"The absolute path to the file to read"},"offset":{"type":"number"},"limit":{"type":"number"}},"required":["filePath"]}}}),
        json!({"type":"function","function":{"name":"write","description":"Write content to a file (creates or overwrites).","parameters":{"type":"object","properties":{"filePath":{"type":"string","description":"The absolute path to the file to write"},"content":{"type":"string"}},"required":["filePath","content"]}}}),
        json!({"type":"function","function":{"name":"edit","description":"Replace exact text in a file.","parameters":{"type":"object","properties":{"filePath":{"type":"string","description":"The absolute path to the file to edit"},"oldText":{"type":"string"},"newText":{"type":"string"}},"required":["filePath","oldText","newText"]}}}),
        json!({"type":"function","function":{"name":"glob","description":"Find files by glob pattern.","parameters":{"type":"object","properties":{"pattern":{"type":"string"},"path":{"type":"string"}},"required":["pattern"]}}}),
        json!({"type":"function","function":{"name":"grep","description":"Regex search file contents.","parameters":{"type":"object","properties":{"pattern":{"type":"string"},"path":{"type":"string"},"include":{"type":"string"}},"required":["pattern"]}}}),
        json!({"type":"function","function":{"name":"todowrite","description":"Create and maintain a structured task list for the current coding session. Use it to track progress during multi-step work and keep todo statuses current.","parameters":{"type":"object","properties":{"todos":{"type":"array","description":"The updated todo list","items":{"type":"object","properties":{"content":{"type":"string"},"status":{"type":"string","description":"pending | in_progress | completed"},"priority":{"type":"string"}},"required":["content","status"]}}},"required":["todos"]}}}),
        json!({"type":"function","function":{"name":"question","description":QUESTION_DESCRIPTION,"parameters":{"type":"object","properties":{"questions":{"type":"array","description":"Questions to ask","items":{"type":"object","properties":{"question":{"type":"string","description":"Complete question"},"header":{"type":"string","description":"Very short label (max 30 chars)"},"options":{"type":"array","description":"Available choices","items":{"type":"object","properties":{"label":{"type":"string","description":"Display text (1-5 words, concise)"},"description":{"type":"string","description":"Explanation of choice"}},"required":["label","description"]}},"multiple":{"type":"boolean","description":"Allow selecting multiple choices"}},"required":["question","header","options"]}}},"required":["questions"]}}}),
    ]
}

/// v1 tool/question.txt — model-facing contract for the question tool
/// (verbatim: prompt_loop routes `question` to the QuestionGate, never to
/// `execute()`).
pub const QUESTION_DESCRIPTION: &str = "Use this tool when you need to ask the user questions during execution. This allows you to:
1. Gather user preferences or requirements
2. Clarify ambiguous instructions
3. Get decisions on implementation choices as you work
4. Offer choices to the user about what direction to take.

Usage notes:
- When `custom` is enabled (default), a \"Type your own answer\" option is added automatically; don't include \"Other\" or catch-all options
- Answers are returned as arrays of labels; set `multiple: true` to allow selecting more than one
- If you recommend a specific option, make that the first option in the list and add \"(Recommended)\" at the end of the label";

/// Read up to `quota` bytes, then KEEP DRAINING (discarding) until EOF so
/// the child never blocks on a full pipe. Memory bound = quota + slack.
/// Both streams get MAX_BYTES: byte-parity with full-capture+head-truncate
/// (the final truncate cuts the concatenated head to MAX_BYTES anyway).
fn drain_limited<R: std::io::Read + Send + 'static>(reader: Option<R>, quota: usize) -> Vec<u8> {
    let Some(mut r) = reader else {
        return Vec::new();
    };
    let mut out: Vec<u8> = Vec::with_capacity(quota.min(64 * 1024));
    let mut total = 0usize;
    let mut buf = [0u8; 16 * 1024];
    loop {
        match r.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                if total < quota {
                    let take = n.min(quota - total);
                    out.extend_from_slice(&buf[..take]);
                    total += take;
                }
                // past quota: these bytes are discarded on the next read
            }
            Err(_) => break,
        }
    }
    out
}

fn truncate(mut output: String) -> (String, bool) {
    let mut truncated = false;
    if output.len() > MAX_BYTES {
        output.truncate(MAX_BYTES);
        // avoid splitting a UTF-8 char
        while !output.is_char_boundary(output.len()) {
            output.pop();
        }
        truncated = true;
    }
    let lines = output.split('\n').count();
    if lines > MAX_LINES {
        let keep: String = output
            .split('\n')
            .take(MAX_LINES)
            .collect::<Vec<_>>()
            .join("\n");
        output = keep;
        truncated = true;
    }
    (output, truncated)
}

/// Execute a tool by v1 name. `cwd` scopes relative paths.
pub fn execute(name: &str, input: &Value, cwd: &Path) -> Result<ToolResult> {
    match name {
        "bash" => bash(input, cwd),
        "read" => read(input, cwd),
        "write" => write(input, cwd),
        "edit" => edit(input, cwd),
        "glob" => glob(input, cwd),
        "grep" => grep(input, cwd),
        "todowrite" => todowrite(input),
        other => bail!("unknown tool: {other}"),
    }
}

fn resolve(cwd: &Path, p: &str) -> PathBuf {
    let path = Path::new(p);
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        cwd.join(path)
    }
}

fn bash(input: &Value, cwd: &Path) -> Result<ToolResult> {
    let command = input["command"].as_str().context("bash.command")?;
    let timeout_ms = input["timeout"].as_u64().unwrap_or(120_000).min(600_000);
    let mut child = std::process::Command::new("sh")
        .arg("-c")
        .arg(command)
        .current_dir(cwd)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .with_context(|| format!("spawn: {command}"))?;
    // K-EFFICIENCY: start the bounded drain BEFORE waiting — otherwise a
    // >pipe-buffer child stalls until the timeout kill (and full-capture
    // ratcheted unbounded bytes into RSS — the OOM chain).
    let so = child.stdout.take();
    let se = child.stderr.take();
    // +1 past MAX_BYTES so the post-concat head-truncate still fires and
    // sets `truncated` exactly like full-capture did
    let h_out = std::thread::spawn(move || drain_limited(so, MAX_BYTES + 1));
    let h_err = std::thread::spawn(move || drain_limited(se, MAX_BYTES + 1));
    // bounded wait (AGENTS §2.3): poll to deadline, kill on overrun
    let deadline = std::time::Instant::now() + std::time::Duration::from_millis(timeout_ms);
    let mut timed_out = false;
    let mut exit_status: Option<std::process::ExitStatus> = None;
    loop {
        match child.try_wait() {
            Ok(Some(st)) => {
                exit_status = Some(st);
                break;
            }
            Ok(None) => {
                if std::time::Instant::now() >= deadline {
                    timed_out = true;
                    let _ = child.kill();
                    let _ = child.wait();
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(25));
            }
            Err(e) => bail!("wait: {e}"),
        }
    }
    let stdout_bytes = h_out.join().unwrap_or_default();
    let stderr_bytes = h_err.join().unwrap_or_default();
    let mut stdout = String::from_utf8_lossy(&stdout_bytes).to_string();
    let stderr = String::from_utf8_lossy(&stderr_bytes).to_string();
    if timed_out {
        if !stdout.ends_with('\n') && !stdout.is_empty() {
            stdout.push('\n');
        }
        stdout.push_str(&format!("[timeout after {timeout_ms}ms]"));
    }
    if !stderr.is_empty() {
        if !stdout.is_empty() && !stdout.ends_with('\n') {
            stdout.push('\n');
        }
        stdout.push_str(&stderr);
    }
    let (output, truncated) = truncate(stdout);
    let code = match exit_status {
        Some(st) => st.code().unwrap_or(-1),
        None => 124, // killed on timeout
    };
    Ok(ToolResult {
        output,
        truncated,
        exit: Some(code),
        title: shell_title(command),
        error: timed_out || code != 0,
        metadata: None,
    })
}

/// v1 bash title: first command's display name (capture: title == command).
fn shell_title(command: &str) -> String {
    command.lines().next().unwrap_or(command).to_string()
}

fn read(input: &Value, cwd: &Path) -> Result<ToolResult> {
    let fp = input["filePath"].as_str().context("read.filePath")?;
    let path = resolve(cwd, fp);
    let content =
        std::fs::read_to_string(&path).with_context(|| format!("read {}", path.display()))?;
    let offset = input["offset"].as_u64().unwrap_or(0) as usize;
    let limit = input["limit"].as_u64().map(|l| l as usize);
    let lines: Vec<&str> = content.split('\n').collect();
    let slice = if offset >= lines.len() {
        ""
    } else {
        let end = match limit {
            Some(l) => (offset + l).min(lines.len()),
            None => lines.len(),
        };
        &lines[offset..end].join("\n")
    };
    let (output, truncated) = truncate(slice.to_string());
    Ok(ToolResult {
        output,
        truncated,
        exit: None,
        title: fp.to_string(),
        error: false,
        metadata: None,
    })
}

fn write(input: &Value, cwd: &Path) -> Result<ToolResult> {
    let fp = input["filePath"].as_str().context("write.filePath")?;
    let content = input["content"].as_str().context("write.content")?;
    let path = resolve(cwd, fp);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).with_context(|| format!("mkdir {}", parent.display()))?;
    }
    std::fs::write(&path, content).with_context(|| format!("write {}", path.display()))?;
    Ok(ToolResult {
        output: format!("Wrote {} bytes to {}", content.len(), fp),
        truncated: false,
        exit: None,
        title: fp.to_string(),
        error: false,
        metadata: None,
    })
}

fn edit(input: &Value, cwd: &Path) -> Result<ToolResult> {
    let fp = input["filePath"].as_str().context("edit.filePath")?;
    let old = input["oldText"].as_str().context("edit.oldText")?;
    let new = input["newText"].as_str().context("edit.newText")?;
    let path = resolve(cwd, fp);
    let content =
        std::fs::read_to_string(&path).with_context(|| format!("read {}", path.display()))?;
    let hits = content.matches(old).count();
    if hits == 0 {
        bail!("oldText not found in {fp}");
    }
    if hits > 1 {
        bail!("oldText matches {hits} times in {fp}; must be unique");
    }
    let updated = content.replacen(old, new, 1);
    std::fs::write(&path, &updated).with_context(|| format!("write {}", path.display()))?;
    Ok(ToolResult {
        output: format!("Edited {fp}"),
        truncated: false,
        exit: None,
        title: fp.to_string(),
        error: false,
        metadata: None,
    })
}

fn glob(input: &Value, cwd: &Path) -> Result<ToolResult> {
    let pattern = input["pattern"].as_str().context("glob.pattern")?;
    let base = match input["path"].as_str() {
        Some(p) => resolve(cwd, p),
        None => cwd.to_path_buf(),
    };
    let mut matches = Vec::new();
    walk(&base, &base, pattern, &mut matches, 0)?;
    matches.sort();
    let joined = matches.join("\n");
    let (output, truncated) = truncate(joined);
    Ok(ToolResult {
        output,
        truncated,
        exit: None,
        title: pattern.to_string(),
        error: false,
        metadata: None,
    })
}

fn walk(base: &Path, dir: &Path, pattern: &str, out: &mut Vec<String>, depth: usize) -> Result<()> {
    if depth > 32 || out.len() > 10_000 {
        return Ok(());
    }
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return Ok(()),
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let rel = path
            .strip_prefix(base)
            .unwrap_or(&path)
            .to_string_lossy()
            .to_string();
        if path.is_dir() {
            if entry.file_name() == ".git" {
                continue;
            }
            walk(base, &path, pattern, out, depth + 1)?;
        } else if wildcard_match(&rel, pattern) {
            out.push(rel);
        }
    }
    Ok(())
}

fn grep(input: &Value, cwd: &Path) -> Result<ToolResult> {
    let pattern = input["pattern"].as_str().context("grep.pattern")?;
    let base = match input["path"].as_str() {
        Some(p) => resolve(cwd, p),
        None => cwd.to_path_buf(),
    };
    let include = input["include"].as_str().map(String::from);
    let re = grep_matcher(pattern)?;
    let mut hits: Vec<String> = Vec::new();
    grep_walk(&base, &base, &re, include.as_deref(), &mut hits, 0)?;
    hits.sort();
    let (output, truncated) = truncate(hits.join("\n"));
    Ok(ToolResult {
        output,
        truncated,
        exit: None,
        title: pattern.to_string(),
        error: false,
        metadata: None,
    })
}

/// regex via the `grep` binary (v1 parity: ripgrep semantics without the dep;
/// spawn cost acceptable — M5 benches decide in-process alternative).
fn grep_matcher(pattern: &str) -> Result<String> {
    Ok(pattern.to_string())
}

fn grep_walk(
    base: &Path,
    dir: &Path,
    pattern: &str,
    include: Option<&str>,
    out: &mut Vec<String>,
    depth: usize,
) -> Result<()> {
    if depth > 32 || out.len() > 10_000 {
        return Ok(());
    }
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return Ok(()),
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            if entry.file_name() == ".git" {
                continue;
            }
            grep_walk(base, &path, pattern, include, out, depth + 1)?;
            continue;
        }
        let rel = path
            .strip_prefix(base)
            .unwrap_or(&path)
            .to_string_lossy()
            .to_string();
        if let Some(inc) = include
            && !wildcard_match(&rel, inc)
        {
            continue;
        }
        let Ok(content) = std::fs::read_to_string(&path) else {
            continue;
        };
        for (i, line) in content.split('\n').enumerate() {
            if regex_search_line(line, pattern) {
                out.push(format!("{}:{}:{}", rel, i + 1, line));
                if out.len() > 10_000 {
                    return Ok(());
                }
            }
        }
    }
    Ok(())
}

/// Substring/regex-lite search: full regex via `grep` binary when available,
/// fallback to substring. Kept honest: no fake regex.
fn regex_search_line(line: &str, pattern: &str) -> bool {
    use std::process::Command;
    let out = Command::new("grep")
        .arg("-E")
        .arg("-q")
        .arg(pattern)
        .arg("-")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .spawn();
    match out {
        Ok(mut child) => {
            use std::io::Write;
            if let Some(stdin) = child.stdin.as_mut() {
                let _ = stdin.write_all(line.as_bytes());
                let _ = stdin.flush();
            }
            drop(child.stdin.take());
            child.wait().map(|s| s.success()).unwrap_or(false)
        }
        Err(_) => line.contains(pattern),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wildcard_quirks_ported() {
        // v1: trailing " *" optional tail
        assert!(wildcard_match("ls", "ls *"));
        assert!(wildcard_match("ls -la", "ls *"));
        // basic globs
        assert!(wildcard_match("*.env", "*.env"));
        assert!(wildcard_match(".env", "*.env"));
        assert!(wildcard_match("a/b.txt", "a/*.txt"));
        assert!(!wildcard_match("a/b.txt", "a/*.md"));
        // escape regex specials literally
        assert!(wildcard_match("file(1).txt", "file(1).txt"));
        // ? single char
        assert!(wildcard_match("abc", "a?c"));
        assert!(!wildcard_match("abbc", "a?c"));
    }

    #[test]
    fn evaluate_last_match_wins_and_default_ask() {
        let rules = vec![
            Rule {
                permission: "*".into(),
                pattern: "*".into(),
                action: "allow".into(),
            },
            Rule {
                permission: "edit".into(),
                pattern: "*.env".into(),
                action: "deny".into(),
            },
        ];
        assert_eq!(evaluate("bash", "rm -rf /", &rules), "allow");
        assert_eq!(evaluate("edit", "/x/.env", &rules), "deny");
        // no matching rule → ask
        assert_eq!(evaluate("question", "x", &[]), "ask");
    }

    #[test]
    fn bash_echo() {
        let r = execute("bash", &json!({"command": "echo TOOL_OK"}), Path::new("/")).unwrap();
        assert_eq!(r.output.trim(), "TOOL_OK");
        assert_eq!(r.exit, Some(0));
        assert!(!r.error);
        assert_eq!(r.title, "echo TOOL_OK");
    }

    #[test]
    fn bash_exit_code_and_stderr_merge() {
        let r = execute(
            "bash",
            &json!({"command": "echo OOPS >&2; exit 3"}),
            Path::new("/"),
        )
        .unwrap();
        assert_eq!(r.exit, Some(3));
        assert!(r.error);
        assert!(r.output.contains("OOPS"));
    }

    #[test]
    fn bash_timeout_enforced() {
        let t0 = std::time::Instant::now();
        let r = execute(
            "bash",
            &json!({"command": "sleep 10", "timeout": 300}),
            Path::new("/"),
        )
        .unwrap();
        assert_eq!(r.exit, Some(124));
        assert!(r.error);
        assert!(t0.elapsed().as_secs() < 5, "timeout must bound the wait");
    }

    #[test]
    fn todowrite_normalizes_and_reports() {
        let r = execute(
            "todowrite",
            &json!({"todos": [
                {"content": "step one", "status": "in_progress"},
                {"content": "step two", "status": "pending", "priority": "high"}
            ]}),
            Path::new("/"),
        )
        .unwrap();
        let meta = r.metadata.expect("todos metadata for the part state");
        assert_eq!(meta["todos"][0]["status"], "in_progress");
        assert_eq!(meta["todos"][1]["priority"], "high");
        assert_eq!(meta["todos"][0]["priority"], "", "priority defaults empty");
        assert!(r.output.contains("step one"));
        assert_eq!(r.title, "todowrite");
        assert!(!r.error);
        // missing todos → Err (model gets a tool failure, not a panic)
        assert!(execute("todowrite", &json!({"nope": []}), Path::new("/")).is_err());
    }

    #[test]
    fn truncation_bounds() {
        let big = "x".repeat(MAX_BYTES + 100);
        let (out, t) = truncate(big);
        assert!(t);
        assert!(out.len() <= MAX_BYTES);
        let many = "\n".repeat(MAX_LINES + 50);
        let (out2, t2) = truncate(many);
        assert!(t2);
        assert!(out2.split('\n').count() <= MAX_LINES + 1);
    }

    #[test]
    fn read_write_edit_roundtrip() {
        let d = tempfile::tempdir().unwrap();
        let fp = d.path().join("f.txt");
        execute(
            "write",
            &json!({"filePath": fp.to_str().unwrap(), "content": "hello world"}),
            d.path(),
        )
        .unwrap();
        let r = execute("read", &json!({"filePath": fp.to_str().unwrap()}), d.path()).unwrap();
        assert_eq!(r.output, "hello world");
        execute(
            "edit",
            &json!({"filePath": fp.to_str().unwrap(), "oldText": "world", "newText": "refine"}),
            d.path(),
        )
        .unwrap();
        let r2 = execute("read", &json!({"filePath": fp.to_str().unwrap()}), d.path()).unwrap();
        assert_eq!(r2.output, "hello refine");
        // duplicate match must fail
        let e = execute(
            "edit",
            &json!({"filePath": fp.to_str().unwrap(), "oldText": "e", "newText": "E"}),
            d.path(),
        );
        assert!(e.is_err(), "non-unique oldText must fail");
    }

    #[test]
    fn glob_and_grep() {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(d.path().join("a.rs"), "fn main() {}\n").unwrap();
        std::fs::create_dir(d.path().join("sub")).unwrap();
        std::fs::write(d.path().join("sub/b.rs"), "fn helper() {}\n").unwrap();
        let g = execute("glob", &json!({"pattern": "*.rs"}), d.path()).unwrap();
        assert!(
            g.output.contains("a.rs") && g.output.contains("sub/b.rs"),
            "{}",
            g.output
        );
        let gr = execute("grep", &json!({"pattern": "helper"}), d.path()).unwrap();
        assert!(gr.output.contains("sub/b.rs:1"), "{}", gr.output);
        let gr2 = execute(
            "grep",
            &json!({"pattern": "main", "include": "*.rs"}),
            d.path(),
        )
        .unwrap();
        assert!(gr2.output.contains("a.rs"), "{}", gr2.output);
    }

    #[test]
    fn schemas_cover_eight_tools() {
        let s = schemas();
        assert_eq!(s.len(), 8, "six exec + question (gated) + todowrite");
        for spec in &s {
            assert_eq!(spec["type"], "function");
            assert!(spec["function"]["name"].as_str().is_some());
            assert!(spec["function"]["parameters"]["type"] == "object");
        }
        assert_eq!(s[6]["function"]["name"], "todowrite");
        assert_eq!(s[7]["function"]["name"], "question");
        let q = &s[7];
        let params = &q["function"]["parameters"];
        assert_eq!(params["required"][0], "questions");
        let item = &params["properties"]["questions"]["items"];
        for f in ["question", "header", "options"] {
            assert!(item["properties"].get(f).is_some(), "Prompt requires {f}");
        }
        assert_eq!(
            item["required"],
            serde_json::json!(["question", "header", "options"]),
            "v1 Prompt required set (custom is Info-only, not in the tool schema)"
        );
        assert!(
            q["function"]["description"]
                .as_str()
                .unwrap()
                .contains("Type your own answer")
        );
    }
}

#[cfg(test)]
mod drain_tests {
    use super::*;
    use std::path::Path;

    /// The OOM/timeout double-bug: the old code read the pipes only AFTER
    /// the child exited, so any output beyond the pipe buffer (~64KB)
    /// stalled the child until the timeout killed it — AND capturing
    /// unbounded bytes first (truncate ran after read_to_end) ratcheted
    /// the transient spike into RSS (16:0x OOM kills, anon 520MB).
    /// Inherent negative control: on the old code this command NEVER
    /// completes (timeout-kill → exit 124 + "[timeout after...]").
    #[test]
    fn huge_output_completes_unblocked_and_stays_bounded() {
        let input = json!({
            "command": "head -c 2000000 /dev/zero | tr '\\0' 'a'",
            "timeout": 5000,
        });
        let r = bash(&input, Path::new("/tmp")).expect("run");
        assert_eq!(
            r.exit,
            Some(0),
            "child must complete — pipes drained concurrently (old code: stall → timeout kill)"
        );
        assert!(
            !r.output.contains("[timeout"),
            "no timeout marker: {:?}",
            &r.output[..80.min(r.output.len())]
        );
        assert!(r.truncated, "output capped");
        assert!(r.output.len() <= MAX_BYTES, "bounded: {}", r.output.len());
    }

    /// small outputs unaffected (byte-parity path)
    #[test]
    fn small_output_untouched() {
        let r = bash(&json!({"command": "printf 'hello\n'"}), Path::new("/tmp")).unwrap();
        assert_eq!(r.exit, Some(0));
        assert_eq!(r.output.trim(), "hello");
        assert!(!r.truncated);
    }
}
