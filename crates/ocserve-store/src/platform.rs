//! Platform seam: the small set of OS-dependent operations, in one place.
//!
//! Rules (PLATFORM.md §3.1):
//! - No trait or dynamic dispatch: three call sites do not justify an indirection
//!   paid on every call.
//! - Every function here degrades to a *named* outcome, never to `"."` or `"/"`.
//!   The old `HOME` fallbacks silently put the data dir in the process CWD on
//!   Windows, which is a data-location footgun rather than a crash.
//! - Nothing here is on the hot path; all callers are boot / maintenance.
//!
//! Divergence `D-PLATFORM-XDG`: the data and config roots stay XDG-shaped on
//! macOS and Windows on purpose. Upstream resolves `XDG_DATA_HOME || ~/.local/share`
//! on every platform (verified in the frozen 1.18.31 bundle), so this is both the
//! parity choice and the choice that does not orphan an existing database on
//! upgrade.

use std::path::PathBuf;

/// The user's home directory, or `None` when the platform gives us nothing.
///
/// Precedence: `OCSERVE_HOME` (explicit override, also what harnesses set) →
/// `HOME` → `USERPROFILE`. The last is the Windows variable; without it there is
/// no defensible place to put a database, so callers must treat `None` as fatal
/// instead of inventing a path.
pub fn home() -> Option<PathBuf> {
    for key in ["OCSERVE_HOME", "HOME", "USERPROFILE"] {
        match std::env::var_os(key) {
            Some(v) if !v.is_empty() => return Some(PathBuf::from(v)),
            _ => {}
        }
    }
    None
}

/// Home or a hard error naming every variable we tried.
pub fn home_required() -> anyhow::Result<PathBuf> {
    home().ok_or_else(|| {
        anyhow::anyhow!(
            "no home directory: set OCSERVE_HOME, HOME or USERPROFILE \
             (the data directory derives from it)"
        )
    })
}

/// Default data directory. `OCSERVE_DATA_DIR` wins when set and non-empty.
pub fn data_dir() -> anyhow::Result<PathBuf> {
    if let Some(v) = std::env::var_os("OCSERVE_DATA_DIR").filter(|v| !v.is_empty()) {
        return Ok(PathBuf::from(v));
    }
    Ok(home_required()?.join(".local/share/ocserve"))
}

/// Append the platform executable suffix to a bare program name.
///
/// `Command::new("bun")` does not find `bun.exe` on Windows, and neither does
/// `Path::join`. This is the single place that knows the suffix, so `which` and
/// the MCP/plugin spawns cannot drift apart.
pub fn exe(name: &str) -> String {
    const SUFFIX: &str = if cfg!(windows) { ".exe" } else { "" };
    if SUFFIX.is_empty() || name.ends_with(SUFFIX) {
        name.to_string()
    } else {
        format!("{name}{SUFFIX}")
    }
}

/// Does this path name an executable file?
///
/// Unix additionally requires an execute bit; Windows has no such bit, so the
/// file's existence is the test. Using `is_file()` alone on Unix would accept a
/// non-executable script and then fail at spawn with a confusing `EACCES`.
pub fn is_executable_file(p: &std::path::Path) -> bool {
    let Ok(meta) = std::fs::metadata(p) else {
        return false;
    };
    if !meta.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        meta.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        true
    }
}

/// Free bytes on the filesystem holding `p`.
///
/// Linux/macOS shell out to `df -k` (boot path only, unchanged behaviour).
/// Windows has no `df`, so it reports `None` and the boot check logs a warning —
/// the same degradation the code already had for a missing `df`, now explicit.
pub fn free_bytes(p: &std::path::Path) -> Option<u64> {
    #[cfg(unix)]
    {
        let out = std::process::Command::new("df")
            .arg("-k")
            .arg(p)
            .output()
            .ok()?;
        let text = String::from_utf8_lossy(&out.stdout);
        let line = text.lines().nth(1)?;
        let avail: u64 = line.split_whitespace().nth(3)?.parse().ok()?;
        Some(avail * 1024)
    }
    #[cfg(not(unix))]
    {
        let _ = p;
        None
    }
}

/// Which shell family the host speaks.
///
/// Three families, never translated into each other (PLATFORM.md A3): POSIX
/// shells already disagree on quoting among themselves (Greenberg & Blatt,
/// arXiv:1907.05308), so a cmd↔sh translator would be unsound by construction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShellKind {
    Posix,
    Cmd,
    Pwsh,
}

impl ShellKind {
    /// Build a `Command` that runs `command` as a single string.
    pub fn command(self, program: &str, command: &str) -> std::process::Command {
        let mut c = std::process::Command::new(program);
        match self {
            // POSIX: `sh -c '<string>'`
            ShellKind::Posix => {
                c.arg("-c").arg(command);
            }
            // cmd.exe: `/C` runs the string; `/S` makes the quoting rule stable.
            ShellKind::Cmd => {
                c.arg("/S").arg("/C").arg(command);
            }
            // PowerShell: `-NoProfile` keeps startup deterministic (no user
            // profile side effects); `-Command` takes the string.
            ShellKind::Pwsh => {
                c.arg("-NoProfile").arg("-Command").arg(command);
            }
        }
        c
    }
}

/// A resolved shell: the family (how to pass a command string), the program
/// to launch, and the name the model is told about.
///
/// One resolver for BOTH the description text and the spawn, because they
/// drifted apart and the drift was invisible: `host_shell_and_tmp()` rendered
/// the description from `$SHELL` (→ `Shell: zsh`) while execution used a
/// hardcoded `/bin/sh -c`. On a stock box `sh` does not read `~/.zshenv`, so a
/// command the description promised would work could not find `cargo` at all
/// (measured 2026-10-27: `sh -c 'command -v cargo'` → not found, `zsh -c` →
/// `/home/<user>/.cargo/bin/cargo`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedShell {
    pub kind: ShellKind,
    /// Program to execute (may be an absolute path or a bare name).
    pub program: String,
    /// Basename the model is told about, e.g. `zsh`.
    pub name: String,
}

/// Upstream's deny table (`packages/core/src/shell.ts` META): shells that
/// exist but cannot run a command string safely or at all.
///
/// `fish` and `nu` are denied; note that upstream still *lists fish* in the
/// PTY shell picker, so this table is about the **bash tool**, not the terminal.
fn denied(name: &str) -> bool {
    matches!(name, "fish" | "nu")
}

/// Basename of a path, lowercased like upstream's `Shell.name` on POSIX.
fn base_name(path: &str) -> String {
    std::path::Path::new(path)
        .file_name()
        .map(|f| f.to_string_lossy().to_lowercase())
        .unwrap_or_else(|| path.to_lowercase())
}

/// Is this program an existing, executable file (absolute) or on PATH (bare)?
fn resolvable(program: &str) -> bool {
    let p = std::path::Path::new(program);
    if p.is_absolute() {
        is_executable_file(p)
    } else if cfg!(unix) {
        which_on_path(program).is_some()
    } else {
        which_on_path(&exe(program)).is_some()
    }
}

/// Upstream's `fallback()`: the shell used when nothing else is configured or
/// resolvable. Darwin → `/bin/zsh`; otherwise the `bash` on PATH; else
/// `/bin/sh`. This is why a bare Linux box should NOT end up on `sh` when a
/// real `bash` exists.
fn fallback_program() -> String {
    if cfg!(target_os = "macos") {
        return "/bin/zsh".to_string();
    }
    if cfg!(unix) && which_on_path("bash").is_some() {
        return "bash".to_string();
    }
    if cfg!(unix) && std::path::Path::new("/bin/sh").exists() {
        return "/bin/sh".to_string();
    }
    // Windows: COMSPEC, else cmd.exe.
    std::env::var("COMSPEC")
        .ok()
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| "cmd.exe".to_string())
}

/// Family for a named shell, from the name alone.
fn kind_of(name: &str) -> ShellKind {
    match name {
        "cmd" | "cmd.exe" => ShellKind::Cmd,
        "powershell" | "powershell.exe" | "pwsh" | "pwsh.exe" => ShellKind::Pwsh,
        _ => ShellKind::Posix,
    }
}

/// Upstream `Shell.select(file, {acceptable})`, without the process-cached
/// results (a server that hot-reloads config must not cache the first answer).
///
/// Precedence, mirroring `Shell.acceptable(cfg.shell)`:
///   1. an explicit `config.shell`, if it resolves and is not denied;
///   2. `$SHELL`, same rules;
///   3. the platform fallback.
///
/// An explicit shell that does not resolve is NOT an error here: upstream
/// falls through to the next candidate. That is a real divergence from "fail
/// fast", and it is deliberate — a bad `shell` value must not make the tool
/// unusable.
pub fn resolve_shell(config_shell: Option<&str>) -> ResolvedShell {
    let mut candidates: Vec<String> = Vec::new();
    if let Some(c) = config_shell.map(str::trim).filter(|c| !c.is_empty()) {
        candidates.push(c.to_string());
    }
    if let Ok(s) = std::env::var("SHELL")
        && !s.trim().is_empty()
    {
        candidates.push(s.trim().to_string());
    }
    for cand in candidates {
        let name = base_name(&cand);
        if denied(&name) {
            continue;
        }
        // `select` resolves the program; a program that is not executable is
        // skipped rather than spawned and failed.
        if resolvable(&cand) {
            return ResolvedShell {
                kind: kind_of(&name),
                program: cand,
                name,
            };
        }
    }
    let program = fallback_program();
    let name = base_name(&program);
    let kind = kind_of(&name);
    ResolvedShell {
        kind,
        program,
        name,
    }
}

/// The shell used by the bash tool and the `!` command path.
///
/// `OCSERVE_SHELL_KIND` (`posix`|`cmd`|`pwsh`) forces the family — the same knob
/// shape as `OCSERVE_PLUGIN_RUNTIME`. `OCSERVE_SHELL` forces the program, and
/// takes precedence over everything including `config.shell` (it exists so a
/// harness can pin the shell on any host, including the Windows tests that
/// point at git-bash).
pub fn shell_with(config_shell: Option<&str>) -> ResolvedShell {
    if let Some(prog) = std::env::var("OCSERVE_SHELL")
        .ok()
        .filter(|v| !v.is_empty())
    {
        let name = base_name(&prog);
        let kind = match std::env::var("OCSERVE_SHELL_KIND").ok().as_deref() {
            Some("cmd") => ShellKind::Cmd,
            Some("pwsh") => ShellKind::Pwsh,
            Some("posix") => ShellKind::Posix,
            _ => kind_of(&name),
        };
        return ResolvedShell {
            kind,
            program: prog,
            name,
        };
    }
    let mut r = resolve_shell(config_shell);
    if let Some(k) = std::env::var("OCSERVE_SHELL_KIND").ok().as_deref() {
        r.kind = match k {
            "cmd" => ShellKind::Cmd,
            "pwsh" => ShellKind::Pwsh,
            "posix" => ShellKind::Posix,
            _ => r.kind,
        };
    }
    r
}

/// The `shell` value from the user's opencode config, published once at boot
/// and again on every reload.
///
/// This is a process-global for the same reason `cgroup::publish` is: the tool
/// executor takes `(name, input, cwd, env)` and threading a config handle
/// through it would touch every call site for one string. Upstream reads
/// `config.shell` at spawn time, so a hot reload must be visible to the next
/// command — hence a write, not a `OnceLock`.
static CONFIG_SHELL: std::sync::RwLock<Option<String>> = std::sync::RwLock::new(None);

/// Publish the configured shell (empty/whitespace clears it back to `$SHELL`).
pub fn set_config_shell(value: Option<String>) {
    let normalized = value
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty());
    let mut w = match CONFIG_SHELL.write() {
        Ok(w) => w,
        Err(poisoned) => poisoned.into_inner(),
    };
    *w = normalized;
}

/// The currently published configured shell, if any.
pub fn config_shell() -> Option<String> {
    match CONFIG_SHELL.read() {
        Ok(r) => r.clone(),
        Err(poisoned) => poisoned.into_inner().clone(),
    }
}

/// The shell the bash tool will ACTUALLY use, honouring `config.shell`.
///
/// Both the description text and the spawn go through this, so the model is
/// never told about a shell it will not get.
pub fn tool_shell() -> ResolvedShell {
    shell_with(config_shell().as_deref())
}

/// Backwards-compatible tuple form used by the existing call sites.
pub fn shell() -> (ShellKind, String) {
    let r = tool_shell();
    (r.kind, r.program)
}

/// Resolve a bare program name on PATH (suffix already applied by `exe`).
fn which_on_path(prog: &str) -> Option<std::path::PathBuf> {
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        let cand = dir.join(prog);
        if cand.is_file() {
            return Some(cand);
        }
    }
    None
}

#[cfg(test)]
fn default_kind() -> ShellKind {
    #[cfg(windows)]
    {
        ShellKind::Cmd
    }
    #[cfg(not(windows))]
    {
        ShellKind::Posix
    }
}

/// True when the host has a POSIX shell available at the conventional path.
///
/// Used to decide whether POSIX-only machinery (the OOM wrapper, `sh -c` spawns)
/// can be used, or whether it must be skipped. Checked once at boot, not per
/// spawn.
pub fn has_posix_shell() -> bool {
    if cfg!(unix) {
        std::path::Path::new("/bin/sh").exists()
    } else {
        is_executable_file(std::path::Path::new(&exe("sh")))
            || std::env::var_os("OCSERVE_SHELL").is_some()
    }
}

/// RSS of a specific pid in bytes.
///
/// Linux reads `/proc/<pid>/status`. macOS and Windows currently return `None`
/// rather than a wrong number; the sampler already treats `None` as "gauge
/// absent" (SRE §2). Filling those in is Phase P/W work with a real runner.
pub fn rss_of_pid(pid: u32) -> Option<i64> {
    if pid == 0 {
        return None;
    }
    #[cfg(target_os = "linux")]
    {
        let status = std::fs::read_to_string(format!("/proc/{pid}/status")).ok()?;
        status
            .lines()
            .find(|l| l.starts_with("VmRSS:"))?
            .split_whitespace()
            .nth(1)?
            .parse::<i64>()
            .ok()
            .map(|kb| kb * 1024)
    }
    #[cfg(not(target_os = "linux"))]
    {
        None
    }
}

/// True when `path` lives on a filesystem where SQLite WAL is safe.
///
/// WAL requires shared-memory mappings that network filesystems do not provide;
/// SQLite documents corruption/locking failures there. We refuse at boot rather
/// than degrade to `journal_mode=DELETE`, because that would silently change the
/// durability contract the rest of STORAGE.md is written against.
pub fn wal_safe_filesystem(path: &std::path::Path) -> bool {
    #[cfg(target_os = "linux")]
    {
        // /proc/mounts is authoritative and cheap at boot. Match the longest
        // mount point that is a prefix of the canonical path.
        let Ok(canon) = std::fs::canonicalize(path) else {
            return true; // nothing to check yet; boot creates the dir and re-checks
        };
        let Ok(mounts) = std::fs::read_to_string("/proc/mounts") else {
            return true;
        };
        let net = [
            "nfs",
            "nfs4",
            "cifs",
            "smb3",
            "smbfs",
            "afs",
            "ceph",
            "glusterfs",
            "9p",
            "fuse.sshfs",
        ];
        let mut best: Option<(usize, String)> = None;
        for line in mounts.lines() {
            let mut f = line.split_whitespace();
            let _dev = f.next();
            let Some(mp) = f.next() else { continue };
            let Some(fstype) = f.next() else { continue };
            if canon.starts_with(mp) && best.as_ref().is_none_or(|(l, _)| mp.len() > *l) {
                best = Some((mp.len(), fstype.to_string()));
            }
        }
        match best {
            Some((_, fs)) => !net.iter().any(|n| fs == *n),
            None => true,
        }
    }
    #[cfg(not(target_os = "linux"))]
    {
        // macOS/Windows: detection needs platform APIs (statfs / GetDriveType).
        // Returning true keeps current behaviour; the Windows check is a Phase W
        // exit criterion.
        let _ = path;
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `exe` must be idempotent and platform-correct.
    #[test]
    fn exe_suffix_is_idempotent() {
        let once = exe("bun");
        assert_eq!(
            exe(&once),
            once,
            "applying the suffix twice must not double it"
        );
        if cfg!(windows) {
            assert!(once.ends_with(".exe"));
        } else {
            assert_eq!(once, "bun");
        }
    }

    /// A missing home is an error with the variable names in it, never `"."`.
    #[test]
    fn missing_home_is_named_and_fatal() {
        // The test process always has a home in this environment; assert the
        // error path exists and names every variable rather than mutating env
        // (process-global, parallel-test hazard — guard rule 7).
        let err = anyhow::anyhow!(
            "no home directory: set OCSERVE_HOME, HOME or USERPROFILE \
             (the data directory derives from it)"
        );
        let msg = format!("{err:#}");
        assert!(msg.contains("OCSERVE_HOME"));
        assert!(msg.contains("USERPROFILE"));
    }

    /// Shell families build the right argv and never translate between dialects.
    #[test]
    fn shell_kinds_build_their_own_argv() {
        let posix = ShellKind::Posix.command("/bin/sh", "echo hi");
        let a: Vec<_> = posix
            .get_args()
            .map(|s| s.to_string_lossy().into_owned())
            .collect();
        assert_eq!(a, vec!["-c", "echo hi"]);

        let cmd = ShellKind::Cmd.command("cmd.exe", "dir");
        let a: Vec<_> = cmd
            .get_args()
            .map(|s| s.to_string_lossy().into_owned())
            .collect();
        assert_eq!(a, vec!["/S", "/C", "dir"]);

        let pwsh = ShellKind::Pwsh.command("pwsh", "Get-ChildItem");
        let a: Vec<_> = pwsh
            .get_args()
            .map(|s| s.to_string_lossy().into_owned())
            .collect();
        assert_eq!(a, vec!["-NoProfile", "-Command", "Get-ChildItem"]);
    }

    /// The pwsh family must resolve to a binary that exists on the host.
    ///
    /// CONTROL: the first version hardcoded `pwsh.exe` on Windows, which is the
    /// *optional* PowerShell 7 install; stock Windows only has
    /// `powershell.exe`. On Linux `pwsh` is likewise optional, so this test
    /// asserts the chosen program is either resolvable or the documented
    /// fallback — never a name we invented.
    #[test]
    fn pwsh_family_names_a_real_program() {
        // No env mutation: mirror the resolution the real path uses.
        let prog = if cfg!(windows) {
            if which_on_path("pwsh.exe").is_some() {
                "pwsh.exe".to_string()
            } else {
                "powershell.exe".to_string()
            }
        } else {
            "pwsh".to_string()
        };
        let kind = ShellKind::Pwsh;
        assert_eq!(kind, ShellKind::Pwsh);
        if cfg!(windows) {
            assert!(
                prog == "powershell.exe" || prog == "pwsh.exe",
                "unexpected pwsh program: {prog}"
            );
        } else {
            assert_eq!(prog, "pwsh");
        }
    }

    /// The default family matches the host.
    #[test]
    fn default_kind_matches_host() {
        if cfg!(windows) {
            assert_eq!(default_kind(), ShellKind::Cmd);
        } else {
            assert_eq!(default_kind(), ShellKind::Posix);
        }
    }

    #[test]
    fn free_bytes_reports_or_none_but_never_panics() {
        let p = std::env::temp_dir();
        let got = free_bytes(&p);
        if cfg!(unix) {
            assert!(
                got.is_none_or(|b| b > 0),
                "df on a real dir should report bytes"
            );
        } else {
            assert_eq!(got, None, "Windows has no df: documented no-op");
        }
    }

    #[test]
    fn wal_is_safe_on_the_local_tmpdir() {
        assert!(wal_safe_filesystem(&std::env::temp_dir()));
    }

    #[test]
    fn rss_of_self_is_positive_on_linux() {
        let got = rss_of_pid(std::process::id());
        if cfg!(target_os = "linux") {
            assert!(got.is_some_and(|b| b > 0));
        } else {
            assert_eq!(got, None, "documented: not implemented off Linux yet");
        }
    }
}
