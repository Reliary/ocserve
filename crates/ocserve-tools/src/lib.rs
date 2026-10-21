//! ocserve-tools: v1 tool names + permission evaluation (PLAN §11: v1 names
//! kept; no v2 renames). Wildcard/evaluate ported from v1 util/wildcard.ts
//! and core/permission.ts; tool params verified against v1 sources + live
//! tool-part capture (testdata/m2).
#![deny(unsafe_code)]

use anyhow::{Context, Result, bail};
pub mod shell_prompt;

use serde_json::{Value, json};
use std::path::{Path, PathBuf};

pub mod bash_patterns;
// The bash tool's spawn/kill surface is the crate's contained unsafe opt-in:
// `libc::killpg` on Unix and a Job Object here on Windows. Each site carries
// its own safety notes; nothing else in the crate is unsafe.
#[cfg(windows)]
#[allow(unsafe_code)]
mod win_job;
#[cfg(windows)]
use win_job::WinJob;
pub mod permission_asks;

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
///
/// Freeze parity: `packages/opencode/src/tool/truncate.ts` at v1.18.31 uses
/// 2000 lines / 50 KiB as DEFAULTS, overridable by `tool_output.{max_lines,
/// max_bytes}`. The frozen shell tool ALSO streams into a file once the raw
/// output passes `limits.maxBytes`, and prepends `...output truncated...
/// Full output saved to: <file>` — so the model keeps access to the rest.
/// ocserve does not have that file sink yet (D-TOOLS-TRUNC-FILE), but the
/// capture ceiling is raised to 1 MiB so a legitimate large listing is not
/// silently cut at 50 KiB before the model ever sees it.
pub const MAX_LINES: usize = 2000;
pub const MAX_BYTES: usize = 1024 * 1024;

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

/// Tools ocserve executes. Advertised tool list = the intersection of these
/// names and the frozen provider-tools.json. `task` is a subagent launcher
/// implemented in `ocserve-core::prompt::task_exec` (K-TASK): the launcher is
/// not a subprocess tool, so it is advertised here but dispatched by the
/// prompt loop (like `question`).
pub const IMPLEMENTED: &[&str] = &[
    "bash",
    "edit",
    "glob",
    "grep",
    "question",
    "read",
    "skill",
    "task",
    "todowrite",
    "webfetch",
    "write",
];

/// Frozen provider tool schemas, captured verbatim from the live 1.18.31
/// provider request (bench/capdiff/frozen/TOOLS.json →
/// scripts/capture-provider-contract.sh). Never hand-edit: a version bump
/// re-captures this file. Descriptions/schemas must be byte-identical to what
/// freeze sends or a model trained on opencode behaves differently
/// (K-CAPDIFF — capdiff diffs both arms' real provider requests).
fn frozen_tools() -> Vec<Value> {
    let raw: Value = serde_json::from_str(include_str!("../assets/provider-tools.json"))
        .expect("vendored provider-tools.json is valid");
    raw.as_array().cloned().unwrap_or_default()
}

/// Provider tool schemas, filtered to IMPLEMENTED (deny filtering happens in
/// the caller). Order matches the frozen capture.
pub fn schemas() -> Vec<Value> {
    let (shell, tmp) = host_shell_and_tmp();
    frozen_tools()
        .into_iter()
        .filter(|t| {
            t.pointer("/function/name")
                .and_then(|n| n.as_str())
                .map(|n| IMPLEMENTED.contains(&n))
                .unwrap_or(false)
        })
        .map(|mut t| {
            if t.pointer("/function/name").and_then(|n| n.as_str()) == Some("bash") {
                // the frozen capture baked THIS host's `${os}/${shell}/${tmp}`;
                // re-render for the running host (K-CAPDIFF)
                let desc = shell_prompt::bash_description(&shell, &tmp);
                t["function"]["description"] = Value::String(desc);
            }
            t
        })
        .collect()
}

/// Host shell basename (upstream `Shell.name`) + model temp dir
/// (upstream `Global.Path.tmp`, which is `<tmp>/opencode`).
fn host_shell_and_tmp() -> (String, String) {
    // The description names the shell the model must write for. On Windows that
    // is cmd, not sh — telling the model "sh" there produces commands that do
    // not parse (PLATFORM.md §3.3).
    let shell = std::env::var("SHELL")
        .ok()
        .filter(|s| !s.is_empty())
        .and_then(|s| {
            std::path::Path::new(&s)
                .file_name()
                .map(|f| f.to_string_lossy().into_owned())
        })
        .unwrap_or_else(|| match ocserve_store::platform::shell() {
            (ocserve_store::platform::ShellKind::Posix, _) => "sh".to_string(),
            (ocserve_store::platform::ShellKind::Cmd, _) => "cmd".to_string(),
            (ocserve_store::platform::ShellKind::Pwsh, _) => "pwsh".to_string(),
        });
    let tmp = std::env::temp_dir()
        .join("opencode")
        .to_string_lossy()
        .into_owned();
    (shell, tmp)
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

/// A per-call stop signal for child-pipe readers.
///
/// 0 nanos = no deadline yet. `close_in` is called by the main thread the
/// moment the DIRECT child exits: the foreground command is over, so anything
/// still holding the pipe is a background job (`... &`), which is supposed to
/// keep running. Readers stop; nothing is killed.
///
/// Why this exists (2026-10-21): `cd x && nohup server &` backgrounds the whole
/// AND-list as a subshell. That subshell inherits fd 1/2 — our pipes — and
/// waits on a server that never exits, so the pipes never reach EOF. The old
/// `join()` therefore blocked a blocking-pool thread forever, and the session
/// lock was never released (one wedged session; sixteen would stall the
/// server).
#[derive(Clone)]
pub struct DrainGate {
    start: std::time::Instant,
    at_nanos: std::sync::Arc<std::sync::atomic::AtomicU64>,
}

impl DrainGate {
    pub fn new() -> Self {
        Self {
            start: std::time::Instant::now(),
            at_nanos: std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0)),
        }
    }

    /// Give readers `d` to drain the residue and reach EOF, then stop.
    pub fn close_in(&self, d: std::time::Duration) {
        let at = self.start.elapsed() + d;
        self.at_nanos.store(
            at.as_nanos().min(u64::MAX as u128) as u64,
            std::sync::atomic::Ordering::Release,
        );
    }

    pub fn is_closed(&self) -> bool {
        let at = self.at_nanos.load(std::sync::atomic::Ordering::Acquire);
        at != 0 && self.start.elapsed().as_nanos() >= at as u128
    }
}

impl Default for DrainGate {
    fn default() -> Self {
        Self::new()
    }
}

/// Read up to `quota` bytes, keep draining (discarding) so the child never
/// blocks on a full pipe, and stop at the gate. Unix polls first so the thread
/// can actually observe the gate; Windows has no pollable anonymous pipes, so
/// it blocks (a lingering reader there is a documented build-only cost).
#[cfg(unix)]
#[allow(unsafe_code)] // contained syscall: poll(2) on our own pipe read end
pub fn drain_until<R: std::io::Read + std::os::fd::AsRawFd + Send + 'static>(
    r: Option<R>,
    quota: usize,
    gate: &DrainGate,
) -> Vec<u8> {
    let Some(mut r) = r else { return Vec::new() };
    let fd = r.as_raw_fd();
    let mut out: Vec<u8> = Vec::with_capacity(quota.min(64 * 1024));
    let mut total = 0usize;
    let mut buf = [0u8; 16 * 1024];
    loop {
        if gate.is_closed() {
            break;
        }
        let mut pfd = libc::pollfd {
            fd,
            events: libc::POLLIN,
            revents: 0,
        };
        // Safety: poll takes a pointer to one initialised pollfd and a count of
        // one; no other memory is touched and the call is thread-safe.
        let ready = unsafe { libc::poll(&mut pfd, 1, 10) };
        if ready <= 0 {
            continue; // timeout or EINTR: re-check the gate
        }
        match r.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                if total < quota {
                    let take = n.min(quota - total);
                    out.extend_from_slice(&buf[..take]);
                    total += take;
                }
            }
            Err(ref e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(_) => break,
        }
    }
    out
}

#[cfg(not(unix))]
pub fn drain_until<R: std::io::Read + Send + 'static>(
    r: Option<R>,
    quota: usize,
    _gate: &DrainGate,
) -> Vec<u8> {
    let Some(mut r) = r else { return Vec::new() };
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
            }
            Err(_) => break,
        }
    }
    out
}

fn truncate(mut output: String) -> (String, bool) {
    let mut truncated = false;
    if output.len() > MAX_BYTES {
        // `String::truncate` PANICS when the index is not a UTF-8 char
        // boundary (the old "repair loop" after it was dead code — the panic
        // fires inside truncate). Find the floor char boundary <= MAX_BYTES
        // first, then truncate. release = panic=abort, so this was an outage.
        let mut cut = MAX_BYTES;
        while cut > 0 && !output.is_char_boundary(cut) {
            cut -= 1;
        }
        output.truncate(cut);
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

/// Execute a tool by v1 name. `cwd` scopes relative paths. `env` carries
/// session environment variables (PUT /api/session/{id}/environment) into
/// subprocess-bearing tools (bash); other tools ignore it. Session vars
/// supplement (never replace) the process environment — `cmd.envs` only
/// adds/overrides the named keys.
pub fn execute(
    name: &str,
    input: &Value,
    cwd: &Path,
    env: &[(String, String)],
) -> Result<ToolResult> {
    match name {
        "bash" => bash(input, cwd, env),
        "read" => read(input, cwd),
        "write" => write(input, cwd),
        "edit" => edit(input, cwd),
        "glob" => glob(input, cwd),
        "grep" => grep(input, cwd),
        "todowrite" => todowrite(input),
        "webfetch" => webfetch(input),
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

#[allow(unsafe_code)] // contained spawn/kill surface: killpg (unix), Job Object (windows)
fn bash(input: &Value, cwd: &Path, env: &[(String, String)]) -> Result<ToolResult> {
    let command = input["command"].as_str().context("bash.command")?;
    let timeout_ms = input["timeout"].as_u64().unwrap_or(120_000).min(600_000);
    // Shell family comes from the host (PLATFORM.md §3.3): POSIX on Unix,
    // COMSPEC on Windows. Command strings are never translated between
    // dialects — the model writes for the shell it is told about.
    let (kind, program) = ocserve_store::platform::shell();
    // Containment (SRE §5.2): a tool's process tree moves itself into
    // kids/tools before it executes anything, so a runaway command (or a
    // `&`-backgrounded one that outlives this call) is capped and reclaimable
    // without competing with the server in `main/`. Observed 2026-10-21: a
    // `nohup cargo build &` stayed in main/ at adj=200 and the kernel killed
    // the server at 321MB while the build held ~600MB.
    let mut cmd = {
        let base = kind.command(&program, command);
        let shape = ocserve_store::cgroup::contained(&program, "tools");
        if shape.program == "sh" {
            let mut wrapped = std::process::Command::new(&shape.program);
            wrapped.args(&shape.prefix);
            wrapped.args(base.get_args());
            if let Some(p) = &shape.cgroup {
                wrapped.env(ocserve_store::cgroup::ENV, p);
            }
            wrapped
        } else {
            base
        }
    };
    cmd.current_dir(cwd)
        .envs(env.iter().map(|(k, v)| (k.as_str(), v.as_str())))
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    // Own process group per invocation (CI 2026-10-08 caught the bug this
    // fixes: killing only `sh` left `sleep`/pipeline grandchildren alive,
    // holding the pipe write-ends, so the bounded-drain joins blocked for the
    // child's full natural duration — and in production a timed-out command
    // kept running invisibly). With the child as group leader, the timeout
    // kill sweeps the whole tree below.
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }
    // Windows: create the child suspended inside a Job Object so the timeout
    // kill can reach the whole tree, and so a crashed server cannot leak tool
    // processes (KILL_ON_JOB_CLOSE). `CREATE_SUSPENDED` closes the
    // spawn-then-assign race: the child cannot run, and therefore cannot fork a
    // grandchild, before it is in the job.
    #[cfg(windows)]
    let job = WinJob::attach(&mut cmd)?;
    let mut child = cmd.spawn().with_context(|| format!("spawn: {command}"))?;
    #[cfg(windows)]
    job.finish(&mut child)?;
    // K-EFFICIENCY: start the bounded drain BEFORE waiting — otherwise a
    // >pipe-buffer child stalls until the timeout kill (and full-capture
    // ratcheted unbounded bytes into RSS — the OOM chain).
    let so = child.stdout.take();
    let se = child.stderr.take();
    // +1 past MAX_BYTES so the post-concat head-truncate still fires and
    // sets `truncated` exactly like full-capture did
    // +1 past MAX_BYTES so the post-concat head-truncate still fires and
    // sets `truncated` exactly like full-capture did.
    let gate = DrainGate::new();
    let h_out = {
        let g = gate.clone();
        std::thread::spawn(move || drain_until(so, MAX_BYTES + 1, &g))
    };
    let h_err = {
        let g = gate.clone();
        std::thread::spawn(move || drain_until(se, MAX_BYTES + 1, &g))
    };
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
                    #[cfg(unix)]
                    {
                        // Safety: `killpg` takes no pointers and is
                        // async-signal-safe. The pid IS the pgid: the spawn
                        // above made the child its own group leader
                        // (`process_group(0)`), so this signals exactly the
                        // command tree we started and nothing else.
                        unsafe {
                            libc::killpg(child.id() as i32, libc::SIGKILL);
                        }
                    }
                    #[cfg(windows)]
                    job.terminate();
                    let _ = child.wait();
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(25));
            }
            Err(e) => bail!("wait: {e}"),
        }
    }
    // The direct child is gone, so the foreground command is done. Anything
    // still holding our pipes is a background job: give the readers a short
    // window to drain the residue and reach EOF, then stop reading. We do NOT
    // kill it — `cd x && nohup server &` is a legitimate way to leave work
    // running, and it is already capped by the kids/tools cgroup.
    gate.close_in(std::time::Duration::from_millis(250));
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
            // saturating: offset/limit come from the model as u64; a large
            // offset near usize::MAX would overflow `offset + l` (debug panic /
            // release wrap-to-huge then `.min(len)` — harmless but UB-adjacent).
            Some(l) => offset.saturating_add(l).min(lines.len()),
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
    // v1 parity (K-CAPDIFF): the model-facing param names are oldString /
    // newString / replaceAll, NOT oldText / newText. A model trained on
    // opencode emits the upstream names, so the old schema silently failed
    // every edit. Accept the upstream names; keep oldText/newText as
    // tolerated aliases (forward-compat, older ocserve clients).
    let old = input["oldString"]
        .as_str()
        .or_else(|| input["oldText"].as_str())
        .context("edit.oldString")?;
    let new = input["newString"]
        .as_str()
        .or_else(|| input["newText"].as_str())
        .context("edit.newString")?;
    let replace_all = input["replaceAll"].as_bool().unwrap_or(false);
    let path = resolve(cwd, fp);
    let content =
        std::fs::read_to_string(&path).with_context(|| format!("read {}", path.display()))?;
    let hits = content.matches(old).count();
    if hits == 0 {
        bail!("oldString not found in {fp}");
    }
    if hits > 1 && !replace_all {
        bail!("oldString matches {hits} times in {fp}; must be unique (or set replaceAll)");
    }
    let updated = if replace_all {
        content.replace(old, new)
    } else {
        content.replacen(old, new, 1)
    };
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

/// webfetch: fetch a URL, return text/markdown/html (v1 tool/webfetch.txt).
/// Bounded: response capped at MAX_BYTES; timeout default 30s (max 120, per
/// the wire schema). markdown = a conservative HTML->text strip (upstream
/// uses turndown; D-WEBFETCH-MD covers conversion differences — the fetch
/// itself is parity).
fn webfetch(input: &Value) -> Result<ToolResult> {
    let url = input["url"].as_str().context("webfetch.url")?;
    let format = input["format"].as_str().unwrap_or("markdown");
    let timeout_s = input["timeout"].as_u64().unwrap_or(30).min(120);
    // v1 webfetch.txt: HTTP URLs are automatically upgraded to HTTPS.
    let url = match url.strip_prefix("http://") {
        Some(rest) => format!("https://{rest}"),
        None => url.to_string(),
    };
    let client = reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(timeout_s))
        .user_agent("ocserve/1.0")
        .build()
        .context("webfetch client")?;
    let resp = client.get(&url).send().context("webfetch request")?;
    let status = resp.status();
    if !status.is_success() {
        bail!("webfetch {url}: HTTP {status}");
    }
    let body = resp.text().context("webfetch body")?;
    let out = match format {
        "html" => body,
        "text" | "markdown" => html_to_text(&body),
        _ => html_to_text(&body),
    };
    let (output, truncated) = truncate(out);
    Ok(ToolResult {
        output,
        truncated,
        exit: None,
        title: url,
        error: false,
        metadata: None,
    })
}

/// Conservative HTML->text: drop script/style, strip tags, decode common
/// entities, collapse blank lines.
fn html_to_text(html: &str) -> String {
    let no_script = strip_blocks(html, "script");
    let no_style = strip_blocks(&no_script, "style");
    let mut out = String::with_capacity(no_style.len());
    let mut in_tag = false;
    for c in no_style.chars() {
        match c {
            '<' => in_tag = true,
            '>' => {
                in_tag = false;
                out.push(' ');
            }
            _ if !in_tag => out.push(c),
            _ => {}
        }
    }
    collapse_lines(&decode_entities(&out))
}

fn strip_blocks(html: &str, tag: &str) -> String {
    let open = format!("<{tag}");
    let close = format!("</{tag}>");
    let mut out = String::with_capacity(html.len());
    let mut rest = html;
    loop {
        match rest.find(&open) {
            Some(i) => {
                out.push_str(&rest[..i]);
                match rest[i..].find(&close) {
                    Some(j) => rest = &rest[i + j + close.len()..],
                    None => break,
                }
            }
            None => {
                out.push_str(rest);
                break;
            }
        }
    }
    out
}

fn decode_entities(s: &str) -> String {
    s.replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&nbsp;", " ")
}

fn collapse_lines(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut blank = 0;
    for line in s.lines() {
        let t = line.trim();
        if t.is_empty() {
            blank += 1;
            if blank > 1 {
                continue;
            }
        } else {
            blank = 0;
        }
        out.push_str(t);
        out.push('\n');
    }
    out.trim_end().to_string()
}

/// Compile the pattern once, like ripgrep does.
///
/// The old implementation spawned `grep -E` **per line** (~0.65 ms/line on this
/// box, so ~0.7 s for a 1k-line file) and, worse, used the POSIX ERE dialect
/// while upstream uses ripgrep's `regex` crate. Measured on this host:
/// `grep -E '\d+'` returns rc 1 (no match) where ripgrep matches "123"; same
/// for `(?i)` and lazy quantifiers. So the model asking for `\d+` silently got
/// zero hits. This function is the fix for both problems.
///
/// `regex` is already linked (rolldown → lazy-regex → regex), so this adds no
/// binary size; see PLATFORM.md Phase G.
fn grep_matcher(pattern: &str) -> Result<regex::Regex> {
    regex::Regex::new(pattern).map_err(|e| anyhow::anyhow!("invalid pattern: {e}"))
}

fn grep_walk(
    base: &Path,
    dir: &Path,
    re: &regex::Regex,
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
            grep_walk(base, &path, re, include, out, depth + 1)?;
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
            if re.is_match(line) {
                out.push(format!("{}:{}:{}", rel, i + 1, line));
                if out.len() > 10_000 {
                    return Ok(());
                }
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod search_engine {
    //! The grep tool must answer with ripgrep semantics, in-process.
    //!
    //! CONTROL (measured): the previous implementation spawned `grep -E` once
    //! per line (0.65 ms/line) and returned no match for `\d+`. Both halves are
    //! asserted here: correctness on the corpus patterns, and that a 1k-line
    //! scan is no longer ~0.7 s of forks.

    use super::*;

    fn repo() -> tempfile::TempDir {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(d.path().join("a.txt"), "abc123\nno digits\n\n").unwrap();
        std::fs::write(d.path().join("b.rs"), "fn main() {\n    let x = 42;\n}\n").unwrap();
        d
    }

    /// `\d+` is the canonical divergence: POSIX ERE has no `\d`.
    #[test]
    fn digit_class_matches_where_grep_ere_did_not() {
        let d = repo();
        let out = execute(
            "grep",
            &serde_json::json!({"pattern": "\\d+"}),
            d.path(),
            &[],
        )
        .unwrap();
        assert!(
            out.output.contains("a.txt:1:abc123"),
            "\\d+ must match a line containing digits: {:?}",
            out.output.as_str()
        );
        assert!(
            !out.output.contains("no digits"),
            "a line without digits must not match: {:?}",
            out.output.as_str()
        );
    }

    #[test]
    fn inline_flags_are_supported() {
        let d = repo();
        let out = execute(
            "grep",
            &serde_json::json!({"pattern": "(?i)ABC"}),
            d.path(),
            &[],
        )
        .unwrap();
        assert!(
            out.output.contains("abc123"),
            "got {:?}",
            out.output.as_str()
        );
    }

    #[test]
    fn invalid_pattern_is_an_error_not_a_substring_fallback() {
        // The deleted helper fell back to `line.contains(pattern)` on spawn
        // failure, which silently turned a bad regex into a literal search.
        let d = repo();
        let err = match execute("grep", &serde_json::json!({"pattern": "a("}), d.path(), &[]) {
            Ok(v) => panic!("invalid regex must fail, got {:?}", v.output),
            Err(e) => e,
        };
        assert!(
            format!("{err:#}").contains("invalid pattern"),
            "got {err:#}"
        );
    }

    /// 1k lines with a hit on the first line: the old code forked 1k times.
    #[test]
    fn thousand_line_scan_is_fast() {
        let d = tempfile::tempdir().unwrap();
        let mut body = String::from("needle here\n");
        for i in 0..1000 {
            body.push_str(&format!("filler line {i}\n"));
        }
        std::fs::write(d.path().join("big.txt"), body).unwrap();
        let t0 = std::time::Instant::now();
        let out = execute(
            "grep",
            &serde_json::json!({"pattern": "needle"}),
            d.path(),
            &[],
        )
        .unwrap();
        let dt = t0.elapsed();
        assert!(out.output.contains("needle here"));
        // 0.65 ms/line * 1001 forks would be ~650 ms. 250 ms is a generous
        // ceiling that still fails by >2x on the old implementation.
        assert!(
            dt < std::time::Duration::from_millis(250),
            "1k-line scan took {dt:?}; the per-line spawn is back"
        );
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
        let r = execute(
            "bash",
            &json!({"command": "echo TOOL_OK"}),
            Path::new("/"),
            &[],
        )
        .unwrap();
        assert_eq!(r.output.trim(), "TOOL_OK");
        assert_eq!(r.exit, Some(0));
        assert!(!r.error);
        assert_eq!(r.title, "echo TOOL_OK");
    }

    #[test]
    fn bash_receives_session_env() {
        // D-ENV unit level: session vars reach the subprocess env; absent
        // keys stay absent (supplement, never replace — PATH survives).
        let env = vec![("OCSERVE_UNIT_PROBE".to_string(), "unit_value_7".to_string())];
        let r = execute(
            "bash",
            &json!({"command": "echo $OCSERVE_UNIT_PROBE; echo $PATH"}),
            Path::new("/"),
            &env,
        )
        .unwrap();
        let mut lines = r.output.lines();
        assert_eq!(lines.next().unwrap_or(""), "unit_value_7");
        assert!(!lines.next().unwrap_or("").is_empty(), "PATH must survive");
        // CONTROL: execute ignoring env → first line empty, red.
    }

    #[test]
    fn bash_exit_code_and_stderr_merge() {
        let r = execute(
            "bash",
            &json!({"command": "echo OOPS >&2; exit 3"}),
            Path::new("/"),
            &[],
        )
        .unwrap();
        assert_eq!(r.exit, Some(3));
        assert!(r.error);
        assert!(r.output.contains("OOPS"));
    }

    /// Regression (2026-10-21): `cd x && nohup server &` backgrounds the whole
    /// AND-list as a subshell that inherits our pipes and waits forever, so a
    /// plain `join()` blocked a blocking-pool thread and the session lock was
    /// never released. The deadline must return the call promptly AND leave
    /// the background job running (we stop reading, we do not kill).
    ///
    /// Driven on a helper thread with a hard receive timeout so a regression
    /// fails the test instead of hanging the suite.
    #[test]
    fn bash_background_subshell_does_not_wedge() {
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let t0 = std::time::Instant::now();
            let r = execute(
                "bash",
                &json!({"command": "cd /tmp && sleep 30 & echo started"}),
                Path::new("/"),
                &[],
            );
            let _ = tx.send((r.map(|x| (x.output, x.exit, x.error)), t0.elapsed()));
        });
        let (res, elapsed) = rx
            .recv_timeout(std::time::Duration::from_secs(10))
            .expect("bash() wedged on a background subshell holding the pipe");
        let (out, exit, err) = res.expect("execute");
        assert!(out.contains("started"), "stdout captured: {out}");
        assert_eq!(exit, Some(0), "foreground exit code preserved");
        assert!(!err, "not an error");
        assert!(
            elapsed < std::time::Duration::from_secs(5),
            "must return promptly, took {elapsed:?}"
        );
        // The background job must SURVIVE (we stop reading; we never kill).
        let alive = std::process::Command::new("sh")
            .arg("-c")
            .arg("pgrep -f '^sleep 30$' >/dev/null")
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        assert!(alive, "background job must outlive the tool call");
        let _ = std::process::Command::new("pkill")
            .args(["-f", "^sleep 30$"])
            .status();
    }

    /// A normal command still returns its exact bytes and exit code.
    #[test]
    fn bash_normal_output_unchanged() {
        let r = execute(
            "bash",
            &json!({"command": "printf 'a\nb\n'"}),
            Path::new("/"),
            &[],
        )
        .unwrap();
        assert_eq!(r.output, "a\nb\n");
        assert_eq!(r.exit, Some(0));
        assert!(!r.truncated);
    }

    #[test]
    fn bash_timeout_enforced() {
        let t0 = std::time::Instant::now();
        let r = execute(
            "bash",
            &json!({"command": "sleep 10", "timeout": 300}),
            Path::new("/"),
            &[],
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
            &[],
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
        assert!(execute("todowrite", &json!({"nope": []}), Path::new("/"), &[]).is_err());
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

    /// 2026-10-11 bug hunt: the old `truncate` called `String::truncate(MAX_BYTES)`
    /// directly, which PANICS when MAX_BYTES is not a UTF-8 char boundary (the
    /// "repair loop" after it was dead code). A file of ASCII + a multibyte char
    /// straddling the cutoff aborted the process (panic=abort). Must now floor to
    /// a boundary and never panic.
    #[test]
    fn truncate_never_panics_on_multibyte_boundary() {
        // MAX_BYTES ASCII bytes then a 3-byte char whose boundary straddles the cap
        let mut s = "a".repeat(MAX_BYTES - 1);
        s.push('€'); // 3 bytes: bytes MAX_BYTES-1, MAX_BYTES, MAX_BYTES+1
        s.push_str(&"b".repeat(100));
        let (out, t) = truncate(s);
        assert!(t);
        assert!(out.len() <= MAX_BYTES);
        assert!(out.is_char_boundary(out.len()));
    }

    #[test]
    fn read_write_edit_roundtrip() {
        let d = tempfile::tempdir().unwrap();
        let fp = d.path().join("f.txt");
        execute(
            "write",
            &json!({"filePath": fp.to_str().unwrap(), "content": "hello world"}),
            d.path(),
            &[],
        )
        .unwrap();
        let r = execute(
            "read",
            &json!({"filePath": fp.to_str().unwrap()}),
            d.path(),
            &[],
        )
        .unwrap();
        assert_eq!(r.output, "hello world");
        execute(
            "edit",
            &json!({"filePath": fp.to_str().unwrap(), "oldText": "world", "newText": "ocserve"}),
            d.path(),
            &[],
        )
        .unwrap();
        let r2 = execute(
            "read",
            &json!({"filePath": fp.to_str().unwrap()}),
            d.path(),
            &[],
        )
        .unwrap();
        assert_eq!(r2.output, "hello ocserve");
        // duplicate match must fail
        let e = execute(
            "edit",
            &json!({"filePath": fp.to_str().unwrap(), "oldText": "e", "newText": "E"}),
            d.path(),
            &[],
        );
        assert!(e.is_err(), "non-unique oldText must fail");
    }

    #[test]
    fn glob_and_grep() {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(d.path().join("a.rs"), "fn main() {}\n").unwrap();
        std::fs::create_dir(d.path().join("sub")).unwrap();
        std::fs::write(d.path().join("sub/b.rs"), "fn helper() {}\n").unwrap();
        let g = execute("glob", &json!({"pattern": "*.rs"}), d.path(), &[]).unwrap();
        assert!(
            g.output.contains("a.rs") && g.output.contains("sub/b.rs"),
            "{}",
            g.output
        );
        let gr = execute("grep", &json!({"pattern": "helper"}), d.path(), &[]).unwrap();
        assert!(gr.output.contains("sub/b.rs:1"), "{}", gr.output);
        let gr2 = execute(
            "grep",
            &json!({"pattern": "main", "include": "*.rs"}),
            d.path(),
            &[],
        )
        .unwrap();
        assert!(gr2.output.contains("a.rs"), "{}", gr2.output);
    }

    #[test]
    fn schemas_are_frozen_and_filtered_to_implemented() {
        // K-CAPDIFF: the provider tool schemas must be byte-identical to what
        // freeze sends for the tools ocserve executes. Guard: every emitted
        // schema equals the vendored frozen entry, and the emitted name set is
        // exactly IMPLEMENTED ∩ frozen (so a new frozen tool is either
        // implemented or explicitly excluded, never silently dropped).
        let s = schemas();
        let frozen: Vec<Value> =
            serde_json::from_str(include_str!("../assets/provider-tools.json")).unwrap();
        let frozen_by: std::collections::BTreeMap<String, &Value> = frozen
            .iter()
            .map(|t| (t["function"]["name"].as_str().unwrap().to_string(), t))
            .collect();
        let mut emitted: Vec<String> = Vec::new();
        for spec in &s {
            let name = spec["function"]["name"].as_str().unwrap().to_string();
            let frozen = frozen_by.get(&name).unwrap();
            // params must be byte-identical to the capture
            assert_eq!(
                &spec["function"]["parameters"], &frozen["function"]["parameters"],
                "{name} params diverged from the frozen capture"
            );
            if name == "bash" {
                // description is HOST-templated upstream — re-rendered for the
                // running host, so assert the render (not the capture)
                let (shell, tmp) = host_shell_and_tmp();
                let want = shell_prompt::bash_description(&shell, &tmp);
                assert_eq!(spec["function"]["description"], serde_json::json!(want));
            } else {
                assert_eq!(
                    &spec["function"]["description"], &frozen["function"]["description"],
                    "{name} description diverged from the frozen capture"
                );
            }
            emitted.push(name);
        }
        let mut want: Vec<String> = IMPLEMENTED
            .iter()
            .filter(|n| frozen_by.contains_key(**n))
            .map(|n| n.to_string())
            .collect();
        want.sort();
        emitted.sort();
        assert_eq!(emitted, want);
        // question: frozen required set + description carry the v1 contract
        let q = frozen_by.get("question").unwrap();
        let params = &q["function"]["parameters"];
        assert_eq!(params["required"][0], "questions");
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
        let r = bash(&input, Path::new("/tmp"), &[]).expect("run");
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
        let r = bash(
            &json!({"command": "printf 'hello\n'"}),
            Path::new("/tmp"),
            &[],
        )
        .unwrap();
        assert_eq!(r.exit, Some(0));
        assert_eq!(r.output.trim(), "hello");
        assert!(!r.truncated);
    }
}
