//! `ocserve.toml` — single-source server config (SRE §5).
//!
//! Minimal surface by design: only `[server]` exists today. Unknown keys are
//! fatal (`deny_unknown_fields`) so a typo'd knob fails at boot instead of
//! silently serving with defaults. Resolution precedence everywhere:
//! explicit flag > `OCSERVE_*` env > this file > built-in default.
//!
//! ```toml
//! [server]
//! port = 4096
//! hostname = "127.0.0.1"
//! auth_mode = "off"          # "off" | "basic"
//! auth_password_file = "/run/secrets/ocserve-pass"
//! ```

use anyhow::{Context, Result};

/// Parsed `[server]` section. All-optional: unset = defer to env/default.
#[derive(Debug, Default, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServerCfg {
    pub port: Option<u16>,
    pub hostname: Option<String>,
    pub auth_mode: Option<String>,
    pub auth_password_file: Option<std::path::PathBuf>,
}

#[derive(Debug, Default, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct File {
    #[serde(default)]
    server: ServerCfg,
}

/// Parse `ocserve.toml` contents (path-free, fuzzable). Missing file is the
/// caller's concern; this is the pure decode+validate core.
pub fn parse(raw: &str) -> Result<ServerCfg> {
    let file: File = toml::from_str(raw).context("parse ocserve.toml")?;
    if let Some(mode) = &file.server.auth_mode
        && mode != "off"
        && mode != "basic"
    {
        anyhow::bail!("[server].auth_mode must be \"off\" or \"basic\", got {mode:?}");
    }
    Ok(file.server)
}

/// Load `ocserve.toml` from the data dir. Missing file = all-defaults
/// (fresh installs boot with zero config — the "bare machine" invariant).
/// Present-but-unparseable = fail-fast with the path and reason.
pub fn load(data_dir: &std::path::Path) -> Result<ServerCfg> {
    let path = data_dir.join("ocserve.toml");
    if !path.exists() {
        return Ok(ServerCfg::default());
    }
    let raw = std::fs::read_to_string(&path).with_context(|| format!("read {}", path.display()))?;
    parse(&raw).with_context(|| format!("parse {}", path.display()))
}

/// Resolve auth mode: flag > `OCSERVE_AUTH_MODE` env > file > "off".
/// Anything other than off/basic (post-trim, case-sensitive) is fail-fast.
pub fn resolve_auth_mode(flag: Option<String>, file: Option<&str>) -> Result<String> {
    if let Some(m) = flag {
        return check_mode(&m);
    }
    match std::env::var("OCSERVE_AUTH_MODE") {
        Ok(v) if !v.trim().is_empty() => return check_mode(v.trim()),
        _ => {}
    }
    if let Some(m) = file {
        return check_mode(m);
    }
    Ok("off".to_string())
}

/// F5 (security review): refuse to bind a NON-loopback address with auth off.
/// The credential store intentionally echoes provider API keys on GET
/// (upstream parity), so an open, network-reachable bind would expose them.
/// Loopback literals (`127.0.0.0/8`, `::1`, `localhost`) are always allowed
/// (the freeze-parity default); any other host with `auth_mode=off` is a
/// misconfiguration and fails fast at boot.
pub fn reject_open_nonloopback(hostname: &str, auth_mode: &str) -> Result<()> {
    if auth_mode != "off" {
        return Ok(());
    }
    let h = hostname
        .trim()
        .trim_start_matches('[')
        .trim_end_matches(']');
    let loopback = h == "localhost"
        || h == "::1"
        || h.parse::<std::net::Ipv4Addr>()
            .map(|ip| ip.is_loopback())
            .unwrap_or(false)
        || h.parse::<std::net::Ipv6Addr>()
            .map(|ip| ip.is_loopback())
            .unwrap_or(false);
    if loopback {
        return Ok(());
    }
    anyhow::bail!(
        "refusing to bind {hostname} with auth_mode=off: the credential store \
         echoes API keys and the server would be reachable off-host. Set \
         auth_mode=\"basic\" (with a password file) or bind a loopback address \
         (the tailnet forwarder pattern keeps the bind loopback)."
    )
}

fn check_mode(m: &str) -> Result<String> {
    if m == "off" || m == "basic" {
        Ok(m.to_string())
    } else {
        anyhow::bail!(
            "auth mode must be \"off\" or \"basic\", got {m:?} (flag > OCSERVE_AUTH_MODE > ocserve.toml)"
        )
    }
}

/// Resolve the basic-auth password: password-file flag > password-file env >
/// file config > `OCSERVE_SERVER_PASSWORD` env. The file must be mode 600
/// (or stricter); content is trimmed of a single trailing newline only.
/// Returns `None` when mode is off (callers must not require it).
pub fn resolve_password(
    mode: &str,
    flag_file: Option<std::path::PathBuf>,
    file_cfg: Option<std::path::PathBuf>,
) -> Result<Option<(String, String)>> {
    if mode == "off" {
        return Ok(None);
    }
    let path = flag_file
        .or_else(|| std::env::var_os("OCSERVE_AUTH_PASSWORD_FILE").map(Into::into))
        .or(file_cfg);
    let secret = match path {
        Some(p) => {
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let meta =
                    std::fs::metadata(&p).with_context(|| format!("stat {}", p.display()))?;
                if meta.permissions().mode() & 0o077 != 0 {
                    anyhow::bail!(
                        "{} is group/world-readable (mode {:o}) — chmod 600 first (rule 8)",
                        p.display(),
                        meta.permissions().mode() & 0o777
                    );
                }
            }
            let raw =
                std::fs::read_to_string(&p).with_context(|| format!("read {}", p.display()))?;
            raw.strip_suffix('\n').unwrap_or(&raw).to_string()
        }
        None => std::env::var("OCSERVE_SERVER_PASSWORD").with_context(|| {
            "auth mode is \"basic\" but no password: set --auth-password-file, \
             OCSERVE_AUTH_PASSWORD_FILE, ocserve.toml auth_password_file, \
             or OCSERVE_SERVER_PASSWORD"
        })?,
    };
    if secret.is_empty() {
        anyhow::bail!("basic-auth password is empty (refusing to serve open)");
    }
    // Upstream v2 fixes the Basic user to `opencode` (pair-v2 authenticates
    // as opencode:<log-password>); we match so clients need no new config.
    Ok(Some(("opencode".to_string(), secret)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mode_precedence_and_rejection() {
        assert_eq!(
            resolve_auth_mode(Some("basic".into()), Some("off")).unwrap(),
            "basic"
        );
        assert_eq!(resolve_auth_mode(None, Some("basic")).unwrap(), "basic");
        assert_eq!(resolve_auth_mode(None, None).unwrap(), "off");
        assert!(resolve_auth_mode(Some("digest".to_string()), None).is_err());
        // CONTROL: permissive fallback (unknown → off) would green-light a
        // typo'd mode into an open server — must stay fail-fast, red.
    }

    #[test]
    fn unknown_toml_key_is_fatal() {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(d.path().join("ocserve.toml"), "[server]\nportx = 1\n").unwrap();
        assert!(load(d.path()).is_err());
        // CONTROL: loader without deny_unknown_fields → Ok, red.
    }

    #[test]
    fn missing_toml_is_defaults() {
        let d = tempfile::tempdir().unwrap();
        let cfg = load(d.path()).unwrap();
        assert!(cfg.port.is_none() && cfg.auth_mode.is_none());
    }

    #[test]
    fn bad_auth_mode_in_toml_is_fatal() {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(
            d.path().join("ocserve.toml"),
            "[server]\nauth_mode = \"digest\"\n",
        )
        .unwrap();
        assert!(load(d.path()).is_err());
    }

    #[test]
    fn off_needs_no_password() {
        assert!(resolve_password("off", None, None).unwrap().is_none());
    }

    #[test]
    fn reject_open_nonloopback_refuses_off_on_public_binds() {
        // loopback literals are fine with auth off
        for h in ["127.0.0.1", "localhost", "::1", "[::1]"] {
            assert!(
                reject_open_nonloopback(h, "off").is_ok(),
                "{h} loopback must be allowed with auth off"
            );
        }
        // any non-loopback host with auth off is a misconfiguration
        for h in [
            "0.0.0.0",
            "::",
            "192.168.1.5",
            "example.com",
            "100.68.229.86",
        ] {
            assert!(
                reject_open_nonloopback(h, "off").is_err(),
                "{h} non-loopback with auth off must fail fast"
            );
        }
        // basic mode allows any bind
        assert!(reject_open_nonloopback("0.0.0.0", "basic").is_ok());
        // CONTROL: a permissive impl (always Ok) would green-light an open
        // public bind — the 0.0.0.0/off case above is the red guard.
    }

    #[test]
    fn basic_without_source_is_fatal() {
        // SAFETY: single-threaded test; env mutation is process-global but no
        // other test reads these keys concurrently under the test harness.
        unsafe {
            std::env::remove_var("OCSERVE_SERVER_PASSWORD");
            std::env::remove_var("OCSERVE_AUTH_PASSWORD_FILE");
        }
        assert!(resolve_password("basic", None, None).is_err());
        // CONTROL: default/empty password fallback → Ok, red.
    }

    #[test]
    fn password_file_mode_enforced() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("pw");
        std::fs::write(&p, "s3cret\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o644)).unwrap();
            assert!(resolve_password("basic", Some(p.clone()), None).is_err());
            std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o600)).unwrap();
            let (u, s) = resolve_password("basic", Some(p), None).unwrap().unwrap();
            assert_eq!((u.as_str(), s.as_str()), ("opencode", "s3cret"));
        }
    }
}
