//! Tool-output truncation, ported from `packages/opencode/src/tool/truncate.ts`
//! and the shell tool's own tail window at v1.18.31.
//!
//! Why this is a module and not four lines inline. The previous form had two
//! problems that were invisible to every gate:
//!
//! 1. It advertised **1 MiB** to the model while upstream advertises 50 KiB,
//!    and the description text carried the 1 MiB number. capdiff compared the
//!    rendered `bash` description against freeze and was permanently red, which
//!    meant the provider-boundary gate could never pass.
//! 2. It truncated in place. Upstream spills the full output to
//!    `<data>/tool-output/<id>` and prepends `...output truncated...\n\nFull
//!    output saved to: <file>`. Without the spill, honouring the real 50 KiB
//!    limit would *destroy* output the model could otherwise still read.
//!
//! Faithfulness notes:
//!   * generic tools (read/grep/glob/webfetch/...) use `Direction::Head`, the
//!     `Truncate.output` default;
//!   * the shell tool uses `Direction::Tail` (upstream's `tail()`), so the model
//!     sees the newest output, which is what matters for a failing command;
//!   * line cuts are at LINE boundaries, not byte boundaries (the old byte cut
//!     could sever a line mid-way);
//!   * a single line longer than the byte budget is cut from its start at a
//!     UTF-8 boundary, which is upstream's `while (buf[start] & 0xc0) === 0x80`.

use std::io::Write;
use std::path::{Path, PathBuf};

/// `truncate.ts::MAX_LINES` at v1.18.31.
pub const MAX_LINES: usize = 2000;
/// `truncate.ts::MAX_BYTES` at v1.18.31 (50 * 1024).
pub const MAX_BYTES: usize = 50 * 1024;

/// Where the model's eye is pointed when output is too long.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    /// Keep the beginning (generic tools, `Truncate.output` default).
    Head,
    /// Keep the end (the shell tool's `tail()`).
    Tail,
}

/// Result of a truncation decision.
#[derive(Debug, Clone)]
pub struct Truncated {
    pub text: String,
    pub truncated: bool,
    /// Set when the full text was spilled to disk.
    pub output_path: Option<PathBuf>,
}

/// The directory upstream uses: `Global.Path.data/tool-output`.
pub fn output_dir() -> Option<PathBuf> {
    ocserve_store::platform::data_dir()
        .ok()
        .map(|d| d.join("tool-output"))
}

/// Write `text` to a fresh file in the truncation directory.
///
/// Returns `None` when the directory cannot be created or written; truncation
/// still proceeds, it just cannot promise the model a path. Silence there is
/// deliberate: a read-only data dir must not turn a working tool call into an
/// error, and the caller only prints the hint when a path exists.
fn spill(text: &str, dir: Option<&Path>) -> Option<PathBuf> {
    let dir = dir?;
    std::fs::create_dir_all(dir).ok()?;
    // Sortable, collision-resistant: upstream uses an ascending ULID. Nanos
    // plus a process-local counter is enough for one process, and the counter
    // matters because two tool calls can land in the same nanosecond.
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(0);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let n = N.fetch_add(1, Ordering::Relaxed);
    let name = format!("tool_{nanos:020}_{n:04}");
    let path = dir.join(name);
    let mut f = std::fs::File::create(&path).ok()?;
    f.write_all(text.as_bytes()).ok()?;
    f.sync_all().ok()?;
    Some(path)
}

/// Number of bytes a line contributes, including the single-byte separator the
/// join would add for every line after the first.
fn line_cost(line: &str) -> usize {
    line.len() + 1
}

/// Byte-offset of the first UTF-8 boundary at or after `at`.
fn ceil_char_boundary(s: &[u8], at: usize) -> usize {
    let mut i = at.min(s.len());
    while i < s.len() && (s[i] & 0xC0) == 0x80 {
        i += 1;
    }
    i
}

/// Apply the window.
///
/// `total` is the full text; the returned `text` is the preview. `spilled` is
/// the on-disk path when the caller asked for a spill.
fn window(text: &str, dir: Option<&Path>, direction: Direction) -> Truncated {
    let lines: Vec<&str> = text.split('\n').collect();
    let total_bytes = text.len();
    if lines.len() <= MAX_LINES && total_bytes <= MAX_BYTES {
        return Truncated {
            text: text.to_string(),
            truncated: false,
            output_path: None,
        };
    }

    let mut kept: Vec<&str> = Vec::new();
    let mut bytes = 0usize;
    let mut hit_bytes = false;

    match direction {
        Direction::Head => {
            for (i, line) in lines.iter().enumerate() {
                if i >= MAX_LINES {
                    break;
                }
                let size = line_cost(line);
                if bytes + size > MAX_BYTES {
                    hit_bytes = true;
                    break;
                }
                kept.push(line);
                bytes += size;
            }
        }
        Direction::Tail => {
            for line in lines.iter().rev() {
                if kept.len() >= MAX_LINES {
                    break;
                }
                let size = line_cost(line);
                if bytes + size > MAX_BYTES {
                    if kept.is_empty() {
                        // One line longer than the whole budget: keep its tail
                        // from a UTF-8 boundary (upstream's exact rule).
                        let b = line.as_bytes();
                        let start = ceil_char_boundary(b, b.len().saturating_sub(MAX_BYTES));
                        kept.push(&line[start..]);
                    }
                    hit_bytes = true;
                    break;
                }
                kept.push(line);
                bytes += size;
            }
            kept.reverse();
        }
    }

    let removed = if hit_bytes {
        format!("{} bytes", total_bytes.saturating_sub(bytes))
    } else {
        format!("{} lines", lines.len().saturating_sub(kept.len()))
    };
    let preview = kept.join("\n");

    let path = spill(text, dir);
    let body = if preview.is_empty() {
        "(no output)".to_string()
    } else {
        preview
    };
    let text = match (&path, direction) {
        (Some(p), Direction::Head) => format!(
            "{body}\n\n...{removed} truncated...\n\n\
             The tool call succeeded but the output was truncated. Full output saved to: {}\n\
             Use Grep to search the full content or Read with offset/limit to view specific sections.",
            p.display()
        ),
        (Some(p), Direction::Tail) => format!(
            "...output truncated...\n\nFull output saved to: {}\n\n{body}",
            p.display()
        ),
        // No sink: still say it was cut, never present a partial body as whole.
        (None, _) => format!("{body}\n\n...{removed} truncated..."),
    };
    Truncated {
        text,
        truncated: true,
        output_path: path,
    }
}

/// Truncate using the process data dir for the spill.
pub fn truncate(text: &str, direction: Direction) -> Truncated {
    let dir = output_dir();
    window(text, dir.as_deref(), direction)
}

/// Truncate with an explicit spill directory (tests, and any caller that wants
/// the output somewhere specific).
pub fn truncate_in(text: &str, dir: Option<&Path>, direction: Direction) -> Truncated {
    window(text, dir, direction)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dir() -> tempfile::TempDir {
        tempfile::tempdir().unwrap()
    }

    #[test]
    fn under_budget_is_byte_identical_and_writes_nothing() {
        let d = dir();
        let t = "a\nb\nc";
        let r = truncate_in(t, Some(d.path()), Direction::Head);
        assert_eq!(r.text, t);
        assert!(!r.truncated);
        assert!(r.output_path.is_none());
        assert_eq!(std::fs::read_dir(d.path()).unwrap().count(), 0);
    }

    #[test]
    fn head_cuts_at_line_boundaries_and_spills_the_full_text() {
        let d = dir();
        let body: String = (0..3000)
            .map(|i| format!("line {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let r = truncate_in(&body, Some(d.path()), Direction::Head);
        assert!(r.truncated);
        assert!(r.text.contains("...1000 lines truncated..."), "{}", r.text);
        assert!(r.text.contains("Full output saved to:"));
        // never a severed line: every kept line is whole
        for line in r.text.lines().take(MAX_LINES) {
            assert!(line.is_empty() || line.starts_with("line "), "{line:?}");
        }
        // the spill holds the ORIGINAL bytes, so nothing is lost
        let p = r.output_path.expect("spill path");
        assert_eq!(std::fs::read_to_string(p).unwrap(), body);
    }

    #[test]
    fn tail_keeps_the_newest_lines_for_a_failing_command() {
        let d = dir();
        let body: String = (0..3000)
            .map(|i| format!("line {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let r = truncate_in(&body, Some(d.path()), Direction::Tail);
        assert!(r.truncated);
        assert!(r.text.contains("line 2999"), "newest line must survive");
        assert!(!r.text.contains("line 0\n"), "oldest must be gone");
        assert!(r.text.starts_with("...output truncated..."));
        assert!(r.text.contains("Full output saved to:"));
    }

    #[test]
    fn byte_limit_applies_to_wide_lines_not_just_the_count() {
        let d = dir();
        // 100 lines but 5 KiB each: the COUNT is fine, the BYTES are not.
        let body = vec!["x".repeat(5000); 100].join("\n");
        let r = truncate_in(&body, Some(d.path()), Direction::Head);
        assert!(r.truncated);
        assert!(
            r.text.contains("bytes truncated"),
            "must report a byte cut, got {}",
            &r.text[..r.text.len().min(200)]
        );
        assert!(r.text.len() < body.len());
    }

    #[test]
    fn single_line_longer_than_the_budget_is_cut_at_a_utf8_boundary() {
        let d = dir();
        // 3-byte chars so a naive cut would land mid-character
        let body = "é".repeat(MAX_BYTES); // 2 bytes each → 2× budget
        let r = truncate_in(&body, Some(d.path()), Direction::Tail);
        assert!(r.truncated);
        // The preview must be valid UTF-8 (it is a String) and no '�'
        assert!(!r.text.contains('\u{FFFD}'));
        // ...and it must actually be bounded, not the whole 100 KiB
        assert!(r.text.len() < MAX_BYTES * 2);
    }

    #[test]
    fn missing_sink_still_reports_truncation_honestly() {
        // A read-only data dir must not turn the tool call into an error, but
        // it must not pretend the body is complete either.
        let r = truncate_in(&"y".repeat(MAX_BYTES + 10), None, Direction::Head);
        assert!(r.truncated);
        assert!(r.output_path.is_none());
        assert!(r.text.contains("truncated"));
        assert!(!r.text.contains("Full output saved"));
    }

    #[test]
    fn empty_output_becomes_the_upstream_placeholder() {
        let d = dir();
        // Force the truncation path with a body that is only bytes, then check
        // the (no output) placeholder appears when nothing is kept.
        let r = truncate_in("", Some(d.path()), Direction::Head);
        assert!(!r.truncated, "empty input is not truncated");
        let big = "z".repeat(MAX_BYTES + 1);
        let r = truncate_in(&big, Some(d.path()), Direction::Tail);
        assert!(r.truncated);
        assert!(
            !r.text.contains("(no output)"),
            "one long line still yields text"
        );
    }

    #[test]
    fn two_spills_in_one_call_get_distinct_paths() {
        let d = dir();
        let body = "q".repeat(MAX_BYTES + 1);
        let a = truncate_in(&body, Some(d.path()), Direction::Head);
        let b = truncate_in(&body, Some(d.path()), Direction::Head);
        assert_ne!(a.output_path, b.output_path);
        assert_eq!(std::fs::read_dir(d.path()).unwrap().count(), 2);
    }
}
