//! Embed the source revision so an operator can tell a locally built binary
//! from a published artifact. Both report the same `0.6.7`; the commit is the
//! difference (2026-10-24 review: a local fix build was indistinguishable from
//! the release artifact during an incident).
use std::process::Command;

fn main() {
    println!("cargo:rerun-if-changed=../../.git/HEAD");
    let commit = Command::new("git")
        .args(["rev-parse", "--short=7", "HEAD"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "unknown".to_string());
    let dirty = Command::new("git")
        .args(["status", "--porcelain"])
        .output()
        .ok()
        .map(|o| !o.stdout.is_empty())
        .unwrap_or(false);
    println!(
        "cargo:rustc-env=OCSERVE_BUILD_COMMIT={commit}{}",
        if dirty { "-dirty" } else { "" }
    );
}
