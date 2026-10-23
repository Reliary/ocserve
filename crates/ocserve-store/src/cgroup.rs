//! Child-process cgroup containment (SRE §5, L2).
//!
//! Placement is the fix; caps are the guardrail. A child that stays in the
//! service's own cgroup competes with the server on equal terms — observed
//! 2026-10-21: a `bash` tool call ran `nohup cargo build &`, the tool returned,
//! the shell exited, and the build kept running in `ocserve.service/main`
//! (no ceiling, `oom_score_adj=200`, same as the server) until the kernel
//! OOM-killed the server at 321 MB while the build held ~600 MB.
//!
//! The kernel primitive that closes this is not a cap: it is *where the child
//! lives*. Every spawn site here puts the child in a per-class leaf under
//! `kids/` before it does any work, and raises its `oom_score_adj` so a memcg
//! kill takes the child (respawnable) rather than the server.
//!
//! Leaf names are a closed set: [`LEAVES`]. Each is an independent service, so
//! `memory.oom.group=1` belongs on the leaf (kill this class as a unit), never
//! on `kids/` (that would kill the browser host because a build spiked).
//!
//! `CLONE_INTO_CGROUP` (clone3) would close the fork→move window atomically,
//! but `std::process::Command` cannot express it and it needs a hand-written
//! spawn path. The window is acceptable because the child is still in the
//! wrapper shell parsing, not allocating (SRE §5.6).

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

/// Classes of child the server spawns. Closed set — a new kind of child needs
/// a new name here, which is the point: it forces the containment question.
pub const LEAVES: [&str; 4] = ["plugin", "mcp", "tools", "pty"];

/// Per-class memory ceiling for a leaf, in MiB.
///
/// These are properties of the child we spawn, not of the user's workload:
/// - plugin: measured warm-up peak 260 MB (bundled embedding stack) + margin
/// - mcp: live peak 122 MB + margin
/// - pty: no measured spike, terminal buffers only
/// - tools: the remainder of the aggregate `kids/` budget; a build may use it
///   all, but cannot take the plugin's share with it.
///
/// `high` sits below `max` so the class is throttled (reclaim and slow) before
/// the kernel kills it. The four `high` values sum to 960M, which is also the
/// parent `MemoryHigh`, so normal concurrent peaks stay under the soft ceiling.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LeafPolicy {
    pub high_mb: u64,
    pub max_mb: u64,
    pub cpu_weight: u64,
}

pub fn leaf_policy(name: &str) -> LeafPolicy {
    match name {
        "plugin" => LeafPolicy {
            high_mb: 288,
            max_mb: 320,
            cpu_weight: 100,
        },
        "mcp" => LeafPolicy {
            high_mb: 160,
            max_mb: 192,
            cpu_weight: 100,
        },
        "pty" => LeafPolicy {
            high_mb: 64,
            max_mb: 96,
            cpu_weight: 100,
        },
        // tools: the remainder of the class budget; a build may use all of it,
        // but cannot take the plugin's share with it.
        _ => LeafPolicy {
            high_mb: 448,
            max_mb: 640,
            cpu_weight: 50,
        },
    }
}

/// The one copy of the child wrapper (guard rule 10 asserts exactly one).
///
/// `echo 500 > oom_score_adj` makes a memcg kill prefer this child over the
/// server (200). The cgroup move is a no-op when no leaf was published
/// (bare/foreground runs, non-systemd, Windows without a POSIX shell).
/// The two `unset` lines matter (2026-10-21): `INVOCATION_ID` is inherited by
/// every descendant, and a tool that runs `ocserve serve` inherited it too,
/// so the nested server believed it was the unit's main process and ran the
/// partition dance inside `kids/tools` (EBUSY, plus a stray `main/` leaf).
/// Unit identity must not leak into tool children.
/// `exec` preserves the pid, so the caller's `killpg`/job-object semantics
/// are unchanged.
pub const WRAPPER: &str = r#"unset INVOCATION_ID SYSTEMD_EXEC_PID 2>/dev/null; echo 500 >/proc/self/oom_score_adj 2>/dev/null; [ -n "$OCSERVE_KIDS_CGROUP" ] && echo $$ >"$OCSERVE_KIDS_CGROUP/cgroup.procs" 2>/dev/null; exec "$@""#;

/// Environment variable carrying the leaf path to the child.
pub const ENV: &str = "OCSERVE_KIDS_CGROUP";

static ROOT: OnceLock<PathBuf> = OnceLock::new();

/// Published by the partition dance (`ocserve-cli/src/partition.rs`) once the
/// hierarchy exists. Idempotent: a second call keeps the first path.
pub fn publish(root: impl Into<PathBuf>) {
    let _ = ROOT.set(root.into());
}

/// The service cgroup root, when the dance ran.
pub fn root() -> Option<&'static Path> {
    ROOT.get().map(|p| p.as_path())
}

/// Path of a class leaf, if the hierarchy was published and the leaf exists.
/// `None` means "not contained": callers fall back to a bare spawn rather than
/// failing, so foreground runs keep working.
pub fn leaf(name: &str) -> Option<PathBuf> {
    let p = ROOT.get()?.join("kids").join(name);
    p.is_dir().then_some(p)
}

/// Whether a POSIX shell exists to run [`WRAPPER`]. Windows uses Job Objects
/// and never takes this path.
pub fn wrapper_usable() -> bool {
    cfg!(unix) && crate::platform::has_posix_shell()
}

/// The shape of a contained spawn, runtime-agnostic.
///
/// `ocserve-plugin` and `ocserve-mcp` spawn with `tokio::process::Command`,
/// `ocserve-tools` and the git callers with `std`. Returning strings instead
/// of a `Command` keeps this crate free of a tokio dependency while leaving
/// exactly one definition of the wrapper.
pub struct Contained {
    /// Program to exec: `sh` when wrapped, otherwise the caller's program.
    pub program: String,
    /// Args to place *before* the caller's own args.
    pub prefix: Vec<String>,
    /// Leaf to export as [`ENV`], when the hierarchy exists.
    pub cgroup: Option<PathBuf>,
}

/// Compute the contained spawn shape for `program` in class `name`.
pub fn contained(program: &str, name: &str) -> Contained {
    let cgroup = leaf(name);
    if wrapper_usable() {
        Contained {
            program: "sh".into(),
            prefix: vec![
                "-c".into(),
                WRAPPER.into(),
                "sh".into(),
                program.to_string(),
            ],
            cgroup,
        }
    } else {
        Contained {
            program: program.to_string(),
            prefix: Vec::new(),
            cgroup,
        }
    }
}

impl Contained {
    /// Apply to a `std::process::Command`: sets the program, prepends the
    /// wrapper args, and exports the leaf when one exists.
    pub fn apply_std(&self, cmd: &mut std::process::Command) {
        cmd.args(&self.prefix);
        match &self.cgroup {
            Some(p) => {
                cmd.env(ENV, p);
            }
            // A spawn with no leaf must NOT inherit an outer leaf path: a
            // nested server or tool would move itself into the ancestor's
            // tree (2026-10-23 review L3).
            None => {
                cmd.env_remove(ENV);
            }
        }
    }
}

/// Build a contained `std::process::Command`. Callers add their own args,
/// cwd and stdio.
pub fn command(program: &str, name: &str) -> std::process::Command {
    let shape = contained(program, name);
    let mut cmd = std::process::Command::new(&shape.program);
    shape.apply_std(&mut cmd);
    cmd
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wrapper_carries_both_moves_and_execs() {
        assert!(WRAPPER.contains("oom_score_adj"), "adj move missing");
        assert!(WRAPPER.contains(ENV), "cgroup move missing");
        assert!(
            WRAPPER.contains("exec \"$@\""),
            "exec missing (pid must survive)"
        );
        assert!(!WRAPPER.contains('\n'), "wrapper must stay a single line");
    }

    #[test]
    fn leaf_names_are_the_closed_set() {
        assert_eq!(LEAVES, ["plugin", "mcp", "tools", "pty"]);
    }

    #[test]
    fn uncontained_returns_none_instead_of_guessing() {
        // no publish() in this test process: every leaf must be absent, and
        // attach() must not set an env var pointing at nothing.
        if root().is_none() {
            assert!(leaf("tools").is_none());
            let shape = contained("true", "tools");
            assert!(shape.cgroup.is_none(), "no leaf on uncontained spawn");
            assert_eq!(shape.program, "sh", "wrapped even when uncontained");
        }
    }

    #[test]
    fn leaf_requires_an_existing_directory() {
        // publish() is process-global and may already be set by another test;
        // only assert the join rule, which is the load-bearing part.
        if let Some(r) = root() {
            let missing = r.join("kids").join("does-not-exist-42");
            assert!(!missing.is_dir());
            assert!(leaf("does-not-exist-42").is_none());
        }
    }
}
