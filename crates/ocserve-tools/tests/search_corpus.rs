//! The search corpus: every pattern must be answered by the `regex` crate the
//! way ripgrep answers it, and the recorded GNU-grep behaviour must be what the
//! old implementation did.
//!
//! Why this exists (PLATFORM.md Phase G): the previous grep tool spawned
//! `grep -E` per line and inherited the POSIX ERE dialect, while upstream uses
//! ripgrep. `\d+` therefore returned **no matches** on "abc123". That is a
//! silent wrong answer, not a performance problem, and it would have survived
//! the engine swap unnoticed without a corpus that pins both sides.
//!
//! Anti-theater: the corpus is data (`bench/search/corpus.json`), not prose. The
//! `gnu_grep` field is the *old* answer; asserting it still reproduces proves
//! the corpus describes a real behaviour change rather than an invented one.

use std::path::Path;

#[derive(serde::Deserialize)]
struct Corpus {
    cases: Vec<Case>,
    #[serde(default)]
    expect_text: std::collections::HashMap<String, ExpectText>,
}

#[derive(serde::Deserialize)]
struct Case {
    pattern: String,
    line: String,
    /// What the `regex` crate (= ripgrep) answers.
    regex: bool,
    /// What GNU `grep -E` answered (the behaviour being fixed).
    gnu_grep: bool,
    #[allow(dead_code)]
    note: String,
}

#[derive(serde::Deserialize)]
struct ExpectText {
    regex: String,
    /// Recorded old-engine answer. Read only by the corpus author when adding
    /// cases; the change itself is proven by the `gnu_grep` test below.
    #[allow(dead_code)]
    gnu_grep: String,
}

fn corpus() -> Corpus {
    let p = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../bench/search/corpus.json");
    let text = std::fs::read_to_string(&p)
        .unwrap_or_else(|e| panic!("corpus missing at {}: {e}", p.display()));
    serde_json::from_str(&text).expect("corpus parses")
}

/// Every corpus case matches the regex crate exactly as recorded.
#[test]
fn regex_crate_matches_the_corpus() {
    let c = corpus();
    assert!(!c.cases.is_empty(), "corpus must not be empty");
    for case in &c.cases {
        let re = regex::Regex::new(&case.pattern)
            .unwrap_or_else(|e| panic!("pattern {:?}: {e}", case.pattern));
        let got = re.is_match(&case.line);
        assert_eq!(
            got, case.regex,
            "pattern {:?} on {:?}: regex crate said {got}, corpus says {}",
            case.pattern, case.line, case.regex
        );
        if let Some(exp) = c.expect_text.get(&case.pattern) {
            let m = re
                .find(&case.line)
                .unwrap_or_else(|| panic!("pattern {:?} had no match", case.pattern));
            assert_eq!(
                m.as_str(),
                exp.regex,
                "pattern {:?}: matched text differs",
                case.pattern
            );
        }
    }
}

/// The corpus faithfully describes a CHANGE: for every case where the two
/// engines disagree, GNU grep must still disagree. If a future grep release
/// grew `\d`, this test tells us the "bug we fixed" narrative is stale.
///
/// Skipped when `grep` is absent (Windows), which is itself the point.
#[test]
fn gnu_grep_still_disagrees_where_the_corpus_says_it_did() {
    if std::process::Command::new("grep")
        .arg("--version")
        .output()
        .is_err()
    {
        eprintln!("skipping: no grep on this host (that is why the engine moved in-process)");
        return;
    }
    let c = corpus();
    for case in &c.cases {
        if case.regex == case.gnu_grep {
            continue; // engines agree; nothing to prove
        }
        let out = std::process::Command::new("grep")
            .arg("-E")
            .arg("-q")
            .arg(&case.pattern)
            .arg("-")
            .stdin(std::process::Stdio::piped())
            .spawn()
            .and_then(|mut ch| {
                use std::io::Write;
                if let Some(mut si) = ch.stdin.take() {
                    si.write_all(case.line.as_bytes())?;
                }
                ch.wait()
            });
        let ok = out.map(|s| s.success()).unwrap_or(false);
        assert_eq!(
            ok, case.gnu_grep,
            "pattern {:?} on {:?}: GNU grep answered {ok}, corpus records {} \
             (update the corpus if the host grep changed)",
            case.pattern, case.line, case.gnu_grep
        );
    }
}
