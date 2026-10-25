//! The operator-facing environment-variable contract.
//!
//! `OCSERVE_*` is a real configuration surface: the systemd unit, the install
//! script and operator runbooks all set it. It is enumerated here, once, so
//! three things stay true:
//!
//! 1. `ocserve doctor --env` shows every knob with its default and the value
//!    currently set in the shell.
//! 2. `docs/ENVIRONMENT.md` is generated from the same table (`ocserve
//!    env-reference`) and checked by guard rule 58, so a new variable cannot
//!    ship undocumented.
//! 3. Tests can tell the operator surface from harness-only knobs.
//!
//! Entries are `&'static`, so the table costs nothing at runtime.

/// Who reads the variable.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Scope {
    /// Normal operator knob, documented in `docs/ENVIRONMENT.md`.
    Operator,
    /// Harness/test-only. Never set on a real deployment.
    Internal,
}

/// One environment variable.
#[derive(Clone, Copy, Debug)]
pub struct Var {
    pub name: &'static str,
    /// Section of the generated reference.
    pub group: &'static str,
    /// Effective value when unset, in words (not necessarily a literal).
    pub default: &'static str,
    /// Accepted values, for a reader who wants to know what to type.
    pub values: &'static str,
    pub scope: Scope,
    /// Value must never be echoed (password/token). Redaction is driven by
    /// this flag, not by the name, so a new secret cannot leak by being
    /// called something unexpected.
    pub secret: bool,
    pub summary: &'static str,
}

const fn op(
    group: &'static str,
    name: &'static str,
    default: &'static str,
    values: &'static str,
    summary: &'static str,
) -> Var {
    Var {
        name,
        group,
        default,
        values,
        scope: Scope::Operator,
        secret: false,
        summary,
    }
}

const fn secret(
    group: &'static str,
    name: &'static str,
    default: &'static str,
    values: &'static str,
    summary: &'static str,
) -> Var {
    Var {
        name,
        group,
        default,
        values,
        scope: Scope::Operator,
        secret: true,
        summary,
    }
}

const fn internal(name: &'static str, default: &'static str, summary: &'static str) -> Var {
    Var {
        name,
        group: "Harness only",
        default,
        values: "(harness)",
        scope: Scope::Internal,
        secret: false,
        summary,
    }
}

/// Every environment variable the server reads. Order is the reference order.
pub const CONTRACT: &[Var] = &[
    // ---- network and files ----
    op(
        "Network and files",
        "OCSERVE_PORT",
        "4096",
        "1-65535",
        "Listen port. Precedence: --port, then ocserve.toml, then this, then 4096.",
    ),
    op(
        "Network and files",
        "OCSERVE_HOSTNAME",
        "127.0.0.1",
        "an address",
        "Bind address. Precedence: --hostname, then ocserve.toml, then this.",
    ),
    op(
        "Network and files",
        "OCSERVE_HOME",
        "HOME, then USERPROFILE",
        "a directory",
        "Overrides the home directory used for config/cache lookups (harnesses set it).",
    ),
    op(
        "Network and files",
        "OCSERVE_DATA_DIR",
        "~/.local/share/ocserve",
        "a directory",
        "State directory: database, blobs, plugin host, snapshots.",
    ),
    op(
        "Network and files",
        "OCSERVE_CONFIG_WRITE",
        "shared",
        "shared | overlay",
        "Where PATCH /config writes. `overlay` keeps ocserve out of the opencode config.",
    ),
    op(
        "Network and files",
        "OCSERVE_CORS",
        "(none)",
        "origin[,origin...]",
        "Extra allowed CORS origins, comma-separated.",
    ),
    // ---- authentication ----
    op(
        "Authentication",
        "OCSERVE_AUTH_MODE",
        "off",
        "off | basic",
        "`basic` challenges every route, including SSE and /metrics.",
    ),
    op(
        "Authentication",
        "OCSERVE_AUTH_PASSWORD_FILE",
        "(none)",
        "a file path",
        "File holding the basic-auth password. Preferred over the env var.",
    ),
    secret(
        "Authentication",
        "OCSERVE_SERVER_PASSWORD",
        "(none)",
        "a password",
        "Basic-auth password inline. Use the file so it stays out of the unit.",
    ),
    // ---- prompts and providers ----
    op(
        "Prompts and providers",
        "OCSERVE_PROMPT_MAX_ROUNDS",
        "0 (unlimited)",
        "0 = unlimited, else a count",
        "Hard cap on model rounds per prompt. Exceeding it surfaces as a turn error.",
    ),
    op(
        "Prompts and providers",
        "OCSERVE_PROMPT_MAX_COST_USD",
        "0 (off)",
        "0 = off, else USD",
        "Hard per-prompt spend ceiling.",
    ),
    op(
        "Prompts and providers",
        "OCSERVE_PROMPT_HISTORY_UNBOUNDED",
        "off (bounded)",
        "0 | 1",
        "1 reads the whole session every turn instead of the proven suffix. Debug only; the sent messages are identical.",
    ),
    op(
        "Prompts and providers",
        "OCSERVE_PROVIDER_STALL_SECS",
        "120",
        "seconds",
        "Stream-stall budget after the first real token.",
    ),
    op(
        "Prompts and providers",
        "OCSERVE_PROVIDER_TTFB_SECS",
        "300",
        "seconds",
        "Budget for the first real token, covering gateway queueing.",
    ),
    op(
        "Prompts and providers",
        "OCSERVE_ZEN_KEYLESS",
        "on",
        "0 disables",
        "Install the keyless `Bearer public` endpoint for opencode free models.",
    ),
    op(
        "Prompts and providers",
        "OCSERVE_CONTINUE_ON_DENY",
        "off (a deny stops the turn)",
        "1 enables",
        "Keep the turn going after a denied tool call.",
    ),
    op(
        "Prompts and providers",
        "OCSERVE_LOOP_GUARD",
        "on",
        "0 disables",
        "Repeated/oscillating tool-call detection and its permission ask.",
    ),
    // ---- plugins and MCP ----
    op(
        "Plugins and MCP",
        "OCSERVE_PLUGIN_RUNTIME",
        "bun when installed, else node",
        "bun | node",
        "Runtime for the plugin host.",
    ),
    op(
        "Plugins and MCP",
        "OCSERVE_PLUGIN_NORMALIZE",
        "on",
        "0 disables",
        "Content-hash-cached plugin normalization before load.",
    ),
    op(
        "Plugins and MCP",
        "OCSERVE_PLUGIN_HEAP_MB",
        "128",
        "MB",
        "V8 heap cap for the Node plugin host.",
    ),
    op(
        "Plugins and MCP",
        "OCSERVE_PLUGIN_TOOLS",
        "on",
        "0 disables",
        "Expose tools declared by plugins to the model.",
    ),
    op(
        "Plugins and MCP",
        "OCSERVE_HOOK_TIMEOUT_MS",
        "per hook: 60 s transform, 180 s tool, 300 s plugin tool",
        "milliseconds; 0 = unbounded",
        "One override for every hook budget.",
    ),
    op(
        "Plugins and MCP",
        "OCSERVE_MCP_TRUST",
        "observe",
        "observe | enforce",
        "`enforce` drops flagged MCP tools and withholds their output.",
    ),
    // ---- storage and memory ----
    op(
        "Storage and memory",
        "OCSERVE_EVENT_RING_MB",
        "32",
        "MB (min 1)",
        "Byte budget for the shared SSE event ring.",
    ),
    op(
        "Storage and memory",
        "OCSERVE_BLOB_CACHE_MB",
        "32",
        "MB",
        "Byte budget for the blob-chunk read cache.",
    ),
    op(
        "Storage and memory",
        "OCSERVE_PAGE_MEMO",
        "on",
        "0 disables",
        "Paged-message response cache.",
    ),
    op(
        "Storage and memory",
        "OCSERVE_PAGE_MEMO_MB",
        "64 (clamped to 64)",
        "MB",
        "Total budget for the paged-message cache.",
    ),
    op(
        "Storage and memory",
        "OCSERVE_LIST_MEMO",
        "on",
        "0 disables",
        "Session-list wire cache.",
    ),
    op(
        "Storage and memory",
        "OCSERVE_SEARCH_MEMO",
        "on",
        "0 disables",
        "Search result cache.",
    ),
    op(
        "Storage and memory",
        "OCSERVE_WIRE_CACHE",
        "on",
        "0 disables",
        "Precomputed response bodies per write epoch.",
    ),
    op(
        "Storage and memory",
        "OCSERVE_TRIM",
        "on",
        "0 disables",
        "Periodic heap trim (returns freed pages to the OS).",
    ),
    op(
        "Storage and memory",
        "OCSERVE_TRIM_SECS",
        "300",
        "seconds; 0 disables",
        "Interval between heap trims.",
    ),
    // ---- cgroups and lifecycle ----
    op(
        "Cgroups and lifecycle",
        "OCSERVE_CGROUP_PARTITION",
        "auto (only under systemd)",
        "0 | 1 | unset",
        "Kids/ cgroup partition for child processes.",
    ),
    op(
        "Cgroups and lifecycle",
        "OCSERVE_KIDS_CGROUP",
        "(internal)",
        "a cgroup directory",
        "Set by the partition dance for child processes; do not set it yourself.",
    ),
    op(
        "Cgroups and lifecycle",
        "OCSERVE_KIDS_MEMORY_MAX_MB",
        "700",
        "32-1048576",
        "Aggregate memory cap for the kids/ cgroup.",
    ),
    op(
        "Cgroups and lifecycle",
        "OCSERVE_SIDECAR_RECYCLE_MB",
        "280",
        "MB; 0 disables",
        "Plugin-host RSS that triggers a graceful restart of the host.",
    ),
    op(
        "Cgroups and lifecycle",
        "OCSERVE_DRAIN_SECS",
        "20",
        "seconds; 0 = forever",
        "Shutdown drain deadline.",
    ),
    // ---- web UI ----
    op(
        "Web UI",
        "OCSERVE_UI",
        "on",
        "0 disables",
        "HTTP UI routes. Disabled returns a JSON 404 instead of the app.",
    ),
    op(
        "Web UI",
        "OCSERVE_UI_PACK",
        "1.18.31",
        "1.18.31 | 2.0.28",
        "Which embedded web-UI build is served.",
    ),
    op(
        "Web UI",
        "OCSERVE_DISABLE_EMBEDDED_WEB_UI",
        "off (embedded build served)",
        "1 disables",
        "Serve through OCSERVE_UI_UPSTREAM instead of the embedded build.",
    ),
    op(
        "Web UI",
        "OCSERVE_UI_UPSTREAM",
        "https://app.opencode.ai",
        "a URL",
        "Proxy target used only when the embedded UI is disabled.",
    ),
    op(
        "Web UI",
        "OCSERVE_UI_PREWARM",
        "off",
        "1 enables",
        "Fetch the entry HTML's assets at boot.",
    ),
    // ---- config, sync and platform ----
    op(
        "Config, sync and platform",
        "OCSERVE_V2",
        "off",
        "1 enables",
        "The 96 v2-only operations (opencode 2 engine and routes).",
    ),
    op(
        "Config, sync and platform",
        "OCSERVE_CONFIG_WATCH",
        "on",
        "0 disables",
        "Config hot-reload poller.",
    ),
    op(
        "Config, sync and platform",
        "OCSERVE_CONFIG_POLL_MS",
        "2000",
        "milliseconds (min 100)",
        "Config poll interval.",
    ),
    op(
        "Config, sync and platform",
        "OCSERVE_LEGACY_SYNC",
        "on",
        "0 disables",
        "Legacy opencode to ocserve delta bridge.",
    ),
    op(
        "Config, sync and platform",
        "OCSERVE_LEGACY_DB",
        "~/.local/share/opencode/opencode.db",
        "a file path",
        "Source database for the delta bridge.",
    ),
    op(
        "Config, sync and platform",
        "OCSERVE_SNAPSHOT",
        "on",
        "0 disables",
        "File snapshots for revert and session diff.",
    ),
    op(
        "Config, sync and platform",
        "OCSERVE_REQ_LOG",
        "off",
        "1 enables",
        "Log every request line. Loud, for debugging only.",
    ),
    op(
        "Config, sync and platform",
        "OCSERVE_SIFT",
        "off",
        "unset/0 | auto | /path/to/sift",
        "Bash-output compression at the provider boundary.",
    ),
    op(
        "Config, sync and platform",
        "OCSERVE_SIFT_TIMEOUT_MS",
        "2000",
        "milliseconds (min 50)",
        "Budget for one sift compression.",
    ),
    op(
        "Config, sync and platform",
        "OCSERVE_SHELL",
        "platform default",
        "a program",
        "Force the shell used by the bash tool.",
    ),
    op(
        "Config, sync and platform",
        "OCSERVE_SHELL_KIND",
        "detected",
        "posix | cmd | pwsh",
        "Force the shell family.",
    ),
    // ---- harness only ----
    internal(
        "OCSERVE_CORPUS",
        "testdata/golden",
        "Replay corpus directory.",
    ),
    internal(
        "OCSERVE_CORPUS_DB",
        "(none)",
        "Database for the bounded-load corpus probe.",
    ),
    internal(
        "OCSERVE_CORPUS_SESSION",
        "(none)",
        "Session for the bounded-load corpus probe.",
    ),
    internal("OCSERVE_PAIR_ALLOW", "(none)", "Pair-check allowlist path."),
    internal(
        "OCSERVE_SA_TIMING_DB",
        "(none)",
        "Disposable database for the migration timing probe.",
    ),
    internal(
        "OCSERVE_TEST_BIND_DELAY_MS",
        "(none)",
        "Debug-build bind delay for shutdown-race tests.",
    ),
    internal(
        "OCSERVE_VERSION",
        "the crate version",
        "Set by the server for the plugin host (`ctx.app.version`).",
    ),
    internal(
        "OCSERVE_BUILD_COMMIT",
        "build.rs",
        "Embedded revision stamp for release builds.",
    ),
];

/// Sections in reference order.
pub const GROUPS: &[&str] = &[
    "Network and files",
    "Authentication",
    "Prompts and providers",
    "Plugins and MCP",
    "Storage and memory",
    "Cgroups and lifecycle",
    "Web UI",
    "Config, sync and platform",
    "Harness only",
];

/// Look up one variable by name.
pub fn find(name: &str) -> Option<&'static Var> {
    CONTRACT.iter().find(|v| v.name == name)
}

/// One row for `ocserve doctor --env`.
pub struct Row {
    pub var: &'static Var,
    pub value: Option<String>,
}

impl Row {
    pub fn is_set(&self) -> bool {
        self.value.is_some()
    }
}

/// Every contract entry with its current process value. Secrets are never
/// echoed, whatever they contain.
pub fn rows() -> Vec<Row> {
    CONTRACT
        .iter()
        .map(|var| {
            let value = std::env::var(var.name).ok();
            let value = value.map(|v| {
                if var.secret {
                    if v.is_empty() {
                        "(empty)".to_string()
                    } else {
                        "(set, redacted)".to_string()
                    }
                } else {
                    v
                }
            });
            Row { var, value }
        })
        .collect()
}

/// Render `docs/ENVIRONMENT.md`. The committed file is generated from this;
/// guard rule 58 fails the build if they differ.
pub fn reference_markdown() -> String {
    let mut out = String::new();
    out.push_str("# Environment variables\n\n");
    out.push_str(
        "<!-- Generated by `ocserve env-reference` from `crates/ocserve-core/src/env_contract.rs`.\n\
         Do not edit by hand: edit the contract, then run\n\
         `cargo run -q -p ocserve -- env-reference > docs/ENVIRONMENT.md`. -->\n\n",
    );
    out.push_str(
        "These are the variables the server reads. `ocserve doctor --env` prints the\n\
         same list and marks the ones set in your shell. Anything not listed here is\n\
         not read by the server; names like `OCSERVE_BIN` belong to repository\n\
         scripts and installers, not to the running service.\n\n",
    );
    out.push_str(
        "Where a knob also exists in `ocserve.toml`, the order is: command-line flag,\n\
         then environment, then file, then default.\n\n",
    );
    out.push_str(
        "A default written as `on` with `0 disables` is a kill switch: leave it unset\n\
         for the normal behaviour.\n\n",
    );

    for group in GROUPS {
        out.push_str(&format!("## {group}\n\n"));
        out.push_str("| Variable | Default | Accepted | Meaning |\n|---|---|---|---|\n");
        for v in CONTRACT.iter().filter(|v| v.group == *group) {
            out.push_str(&format!(
                "| `{}` | {} | {} | {} |\n",
                v.name,
                escape_cell(v.default),
                escape_cell(v.values),
                escape_cell(v.summary)
            ));
        }
        out.push('\n');
    }
    out
}

fn escape_cell(s: &str) -> String {
    s.replace('|', "\\|")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_are_unique_and_prefixed() {
        let mut seen = std::collections::HashSet::new();
        for v in CONTRACT {
            assert!(
                v.name.starts_with("OCSERVE_"),
                "{} must be OCSERVE_*",
                v.name
            );
            assert!(seen.insert(v.name), "duplicate contract entry {}", v.name);
            assert!(GROUPS.contains(&v.group), "{} has unknown group", v.name);
            assert!(!v.summary.is_empty(), "{} needs a summary", v.name);
            assert!(!v.default.is_empty(), "{} needs a default", v.name);
            assert!(!v.values.is_empty(), "{} needs accepted values", v.name);
        }
    }

    #[test]
    fn every_group_is_used() {
        for g in GROUPS {
            assert!(CONTRACT.iter().any(|v| v.group == *g), "group {g} is empty");
        }
    }

    #[test]
    fn find_is_exact() {
        assert!(find("OCSERVE_PORT").is_some());
        assert!(find("OCSERVE_PORTX").is_none());
        assert!(find("PORT").is_none());
    }

    #[test]
    fn secret_named_vars_are_flagged() {
        for v in CONTRACT {
            let upper = v.name.to_ascii_uppercase();
            // `*_FILE` holds a path, not a secret; `*_PASSWORD`,
            // `*_KEY`, `*_TOKEN`, `*_SECRET` hold values.
            let looks_secret = !upper.ends_with("_FILE")
                && (upper.contains("PASSWORD")
                    || upper.ends_with("_KEY")
                    || upper.contains("TOKEN")
                    || upper.contains("SECRET"));
            if looks_secret {
                assert!(v.secret, "{} looks secret but is not flagged", v.name);
            }
        }
        assert!(
            find("OCSERVE_SERVER_PASSWORD").is_some_and(|v| v.secret),
            "the server password must be redacted"
        );
    }

    #[test]
    fn password_is_never_echoed() {
        // The mapping must render a marker, not the value, whenever it is set.
        let row = rows()
            .into_iter()
            .find(|r| r.var.name == "OCSERVE_SERVER_PASSWORD")
            .expect("password is in the contract");
        if let Some(v) = row.value {
            assert!(
                v.starts_with("(set") || v == "(empty)",
                "password must be redacted, got {v:?}"
            );
        }
    }

    #[test]
    fn reference_lists_every_var_once_and_in_its_own_group() {
        let md = reference_markdown();
        for v in CONTRACT {
            assert_eq!(
                md.matches(&format!("`{}`", v.name)).count(),
                1,
                "{} must appear exactly once",
                v.name
            );
        }
        let harness = md.find("## Harness only").unwrap();
        for v in CONTRACT.iter().filter(|v| v.scope == Scope::Internal) {
            assert!(
                md[harness..].contains(&format!("`{}`", v.name)),
                "{} is internal and must appear only under Harness only",
                v.name
            );
        }
        let operator_part = &md[..harness];
        for v in CONTRACT.iter().filter(|v| v.scope == Scope::Operator) {
            assert!(
                operator_part.contains(&format!("`{}`", v.name)),
                "{} must appear in an operator section",
                v.name
            );
        }
    }

    #[test]
    fn internal_entries_are_all_in_the_harness_group() {
        for v in CONTRACT.iter().filter(|v| v.scope == Scope::Internal) {
            assert_eq!(v.group, "Harness only", "{}", v.name);
        }
    }
}
