//! Zen free-tier request shaping (bench/zen-probe/FINDINGS: P5/P6/P7/P8).
//!
//! The opencode.ai free-tier gate requires (1) the composite UA — set in
//! `refine-llm` per request — and (2) a `tools` array in the body whenever
//! the request carries `Authorization: Bearer public` (P5 passes with both,
//! P7 fails without the UA, P6 fails without tools). Text-only calls —
//! auto-compaction summaries and tools-off finalize rounds — must therefore
//! still SEND tools, with `tool_choice: "none"` so the model can never call
//! them (P8).

/// `tool_choice` for a zen request: `"none"` on text-only calls (tools are
/// present only to satisfy the gate), `None` → default `"auto"` when the
/// round's real tools are enabled.
pub fn tool_choice_for(is_zen: bool, tools_enabled: bool) -> Option<String> {
    if is_zen && !tools_enabled {
        Some("none".to_string())
    } else {
        None
    }
}

/// Whether a text-only call must fall back to the builtin tool schemas to
/// satisfy the gate (`is_zen && !tools_enabled`).
pub fn needs_fallback_tools(is_zen: bool, tools_enabled: bool) -> bool {
    is_zen && !tools_enabled
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_choice_none_only_on_zen_text_only_calls() {
        assert_eq!(
            tool_choice_for(true, false).as_deref(),
            Some("none"),
            "compaction/tools-off round on zen must pin none (P8)"
        );
        assert_eq!(tool_choice_for(true, true), None, "real tools keep auto");
        assert_eq!(
            tool_choice_for(false, false),
            None,
            "non-zen providers untouched (deepseek summarization unchanged)"
        );
    }

    #[test]
    fn fallback_tools_only_on_zen_text_only_calls() {
        assert!(needs_fallback_tools(true, false));
        assert!(!needs_fallback_tools(true, true));
        assert!(
            !needs_fallback_tools(false, false),
            "non-zen never falls back"
        );
        assert!(!needs_fallback_tools(false, true));
    }
}
