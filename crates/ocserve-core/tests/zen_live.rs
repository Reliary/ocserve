//! Live zen free-tier gate checks (nightly; PR runs skip via `#[ignore]`).
//!
//! The trio is ANTI-THEATER by design (TESTING §1):
//! - positive: the production wire completes (FINDINGS P5 + B6/B7 —
//!   composite UA + native session id + **≥2 known opencode tool names**;
//!   max_tokens is NOT required — B5),
//! - text-only: compaction/tools-off shape (tools + `tool_choice: none`,
//!   FINDINGS P8) — this is the M6 summary path on a zen default,
//! - negative: malformed 64-hex session id still trips the gate (E9/E4) —
//!   proves the wall still EXISTS (if it vanishes, this goes red too).
//!
//! Introduction run 2026-10-05: positive 200 / text-only 200 / negative 403.

use futures_util::StreamExt;
use ocserve_llm::{ChatMessage, ChatOpts, Client};
use serde_json::Value;

/// The REAL advertised tool set. The gate tightened on 2026-10-25: a
/// hand-built 8-tool fixture that used to pass now 429s, while the production
/// wire (real base prompt + real tool schemas) still passes. Using the
/// production functions here is the only way this fixture cannot drift from
/// what the server sends.
fn gate_tools() -> Vec<Value> {
    ocserve_tools::schemas()
}

/// The production system prompt (vendored base + env + skills; ~10 KB).
/// A short stand-in is rejected by the gate (3-round bisect), so the fixture
/// uses the same function the server does.
fn gate_system() -> ChatMessage {
    let base = ocserve_core::system_prompt::base_prompt(None, "big-pickle", "opencode");
    let env = ocserve_core::system_prompt::environment_block(
        "big-pickle",
        "opencode",
        "/probe",
        "/probe",
        false,
        "linux",
        "2026-10-25T00:00:00.000Z",
    );
    let skills = ocserve_core::system_prompt::skills_block(&[(
        "demo".to_string(),
        "A demo skill entry so the block renders like production.".to_string(),
        "/probe/skills/demo/SKILL.md".to_string(),
    )]);
    let composed = ocserve_core::system_prompt::compose(&base, &[Some(env), skills], None);
    ChatMessage::text("system", composed)
}

/// ocserve ids.rs shape: `ses_` + 26 hex chars (proven accepted, P5).
const NATIVE_SID: &str = "ses_0123456789abcdef0123456789";

async fn first_event(opts: ChatOpts, label: &str) -> Result<(), anyhow::Error> {
    let client = Client::new("https://opencode.ai/zen/v1", "public");
    // The gate also requires a system message (verified 2026-10-25 across 3
    // rounds: 0 system messages => 429, 1 or 2 => 200, byte-identical
    // otherwise). Production always sends one; this fixture now matches it.
    let msgs = vec![
        gate_system(),
        ChatMessage::text("user", "Reply with exactly: ZEN_OK"),
    ];
    let mut stream = client
        .chat_stream(
            "big-pickle",
            &msgs,
            &opts,
            Some(&gate_tools()),
            Some(NATIVE_SID),
        )
        .await?;
    match stream.next().await {
        Some(Ok(_)) => {
            println!("{label}: HIT");
            Ok(())
        }
        Some(Err(e)) => Err(e),
        None => anyhow::bail!("{label}: stream EOF before any event"),
    }
}

#[tokio::test]
#[ignore = "live zen gate (nightly; 1 request)"]
async fn zen_keyless_completion_succeeds() {
    first_event(ChatOpts::default(), "positive")
        .await
        .expect("gate should accept the proven production wire (FINDINGS P5/B6)");
}

/// M6 compaction/tools-off shape on a zen default: tools present + pinned
/// `tool_choice: none` (P8) must both satisfy the gate and yield events.
#[tokio::test]
#[ignore = "live zen gate text-only shape (nightly; 1 request)"]
async fn zen_text_only_summary_shape_succeeds() {
    let opts = ChatOpts {
        tool_choice: Some("none".into()),
        ..Default::default()
    };
    first_event(opts, "text-only")
        .await
        .expect("compaction wire (tools + tool_choice none) must pass (P8)");
}

#[tokio::test]
#[ignore = "live zen gate negative control (nightly; 1 request)"]
async fn zen_gate_still_rejects_malformed_session() {
    let client = Client::new("https://opencode.ai/zen/v1", "public");
    let msgs = vec![gate_system(), ChatMessage::text("user", "say ok")];
    let malformed = format!("ses_{}", "a".repeat(64));
    let err = client
        .chat_stream(
            "big-pickle",
            &msgs,
            &ChatOpts::default(),
            Some(&gate_tools()),
            Some(&malformed),
        )
        .await;
    // BoxStream is not Debug — match instead of expect_err.
    let e = match err {
        Ok(_) => panic!("64-hex session id must still trip the gate (E9/E4)"),
        Err(e) => e,
    };
    // The wall has been renamed over time (`FreeTierError` -> 2026-10-25
    // `FreeUsageLimitError`); the invariant is that a malformed session id is
    // REJECTED, and the body names a free-tier wall.
    let msg = format!("{e:#}");
    assert!(
        msg.contains("FreeTierError") || msg.contains("FreeUsageLimitError"),
        "expected a free-tier wall rejection, got: {msg}"
    );
}
