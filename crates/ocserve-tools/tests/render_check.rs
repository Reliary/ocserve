//! The rendered `bash` description must match the frozen provider capture.
//!
//! Why a test and not only capdiff: capdiff compares against a live freeze
//! binary and runs in the nightly, so a description regression is invisible to
//! every per-commit gate. This ran red once already: the description advertised
//! `1048576` bytes while freeze advertised `51200`, and the provider-boundary
//! differential could never pass.
//!
//! Two values are HOST-templated upstream and therefore legitimately differ
//! from the capture: `${shell}` (the user's shell) and `${tmp}` (`TMPDIR`).
//! They are normalized here; everything else must be byte-identical.

fn normalize(mut s: String, tmp: &str) -> String {
    // Both sides may have baked a different temp root: the capture recorded
    // whatever TMPDIR the capture run used, and this test runs under whatever
    // TMPDIR the suite has. Replace the KNOWN literal roots longest-first and
    // then this host's path, so neither direction leaves a dangling prefix
    // (`/var<tmp>` — observed 2026-11-04 when the 1.19.5 capture ran under
    // /var/tmp but cargo test defaulted to /tmp).
    for known in ["/var/tmp/opencode", "/tmp/opencode"] {
        s = s.replace(known, "<tmp>");
    }
    s = s.replace(tmp, "<tmp>");
    s
}

#[test]
fn description_matches_frozen_capture() {
    let frozen: Vec<serde_json::Value> =
        serde_json::from_str(include_str!("../assets/provider-tools.json")).unwrap();
    let want = frozen
        .iter()
        .find(|t| t["function"]["name"] == "bash")
        .expect("bash in capture")["function"]["description"]
        .as_str()
        .unwrap()
        .to_string();

    let shell = ocserve_store::platform::tool_shell().name;
    let tmp = std::env::temp_dir()
        .join("opencode")
        .to_string_lossy()
        .into_owned();
    let got = ocserve_tools::shell_prompt::bash_description(&shell, &tmp);

    // The capture was recorded on linux/zsh; on any other host the shell name
    // is a legitimate difference, so compare with that line normalized too.
    let want = normalize(want.replace("Shell: zsh", "Shell: <shell>"), &tmp);
    let got = normalize(
        got.replace(&format!("Shell: {shell}"), "Shell: <shell>"),
        &tmp,
    );
    assert_eq!(
        got, want,
        "bash description diverged from the frozen capture"
    );
}

#[test]
fn advertised_limits_are_the_enforced_limits() {
    // The description tells the model when output is truncated. If the numbers
    // in the text and the constants in the truncator disagree, one of them is
    // lying about what the tool will do.
    let d = ocserve_tools::shell_prompt::bash_description("zsh", "/tmp/opencode");
    assert!(
        d.contains(&format!(
            "exceeds {} lines or {} bytes",
            ocserve_tools::MAX_LINES,
            ocserve_tools::MAX_BYTES
        )),
        "description must state the enforced limits ({}/{})",
        ocserve_tools::MAX_LINES,
        ocserve_tools::MAX_BYTES
    );
    assert_eq!(ocserve_tools::MAX_BYTES, 50 * 1024, "freeze parity");
    assert_eq!(ocserve_tools::MAX_LINES, 2000, "freeze parity");
}
