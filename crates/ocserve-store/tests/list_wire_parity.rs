//! M1 wire-parity contract: `build_sessions_wire_bytes` must emit EXACTLY
//! what `serde_json::to_vec(&load_sessions_wire(db)?)` emits.
//!
//! The session list is a byte-golden freeze surface (oc-remote and the TUI
//! parse it), so a formatting difference here is a wire violation. M1
//! hand-writes the members to avoid building 201 `Value` trees (measured
//! 684 µs per request on the fixture), so this test is what makes that safe.
//!
//! Adversarial column population on purpose: NULLs, empty strings, unicode,
//! quote/backslash/control characters in titles, negative and fractional
//! costs, zero and large counters, a `model` blob stored with non-compact
//! whitespace, and a `model` column that is not JSON at all (the fallback).

use ocserve_store::writer::Writer;

fn seed(dir: &std::path::Path) -> std::path::PathBuf {
    let db = dir.join("t.db");
    {
        let conn = ocserve_store::pragma::create_new(&db).unwrap();
        ocserve_store::schema::migrate(&conn).unwrap();
        conn.execute_batch(
            "INSERT INTO session (id, project_id, directory, path, slug, title, version,
                                  agent, model, cost, time_created, time_updated)
             VALUES ('ses_a','p1','/w','s','slug','title A','1.18.31','build',
                     '{\"id\":\"deepseek\",\"providerID\":\"deepseek\",\"variant\":\"default\"}',
                     0.25, 100, 200);",
        )
        .unwrap();
        // every nullable/odd column variant in one row
        conn.execute_batch(
            "INSERT INTO session (id, project_id, directory, path, slug, title, version,
                                  cost, time_created, time_updated)
             VALUES ('ses_b','p2','','s2','s','quote \" backslash \\\\ tab \t ctrl \u{1} unicode \u{e9}\u{4e2d}\u{1f600}',
                     '1.18.31', -1.5, 1, 2);",
        )
        .unwrap();
        // model stored with pretty whitespace (must normalize) and a NULL model
        conn.execute_batch(
            "INSERT INTO session (id, project_id, directory, path, slug, title, version,
                                  model, cost, time_created, time_updated)
             VALUES ('ses_c','p3','/w','s3','s','C','1.18.31',
                     '{\"id\": \"x\", \"providerID\": \"y\", \"variant\": \"default\"}', 0.0, 3, 3);
             INSERT INTO session (id, project_id, directory, path, slug, title, version,
                                  model, cost, time_created, time_updated)
             VALUES ('ses_d','p4','/w','s4','s','D','1.18.31', 'not json at all', 1e21, 4, 4);
             INSERT INTO session (id, project_id, directory, path, slug, title, version,
                                  model, cost, time_created, time_updated)
             VALUES ('ses_e','p5','/w','s5','s','E','1.18.31', NULL, 0.1, 5, 5);",
        )
        .unwrap();
        // big/zero counters (summary + tokens are NOT NULL DEFAULT 0)
        conn.execute_batch(
            "UPDATE session SET summary_additions = -3, summary_deletions = 9223372036854775807,
                                summary_files = 0, tokens_input = 9007199254740993,
                                tokens_output = 1, tokens_reasoning = 0,
                                tokens_cache_read = 4503599627370496, tokens_cache_write = 0
             WHERE id = 'ses_a';",
        )
        .unwrap();
    }
    db
}

#[test]
fn m1_wire_bytes_match_the_dom_path() {
    let dir = tempfile::tempdir().unwrap();
    let db = seed(dir.path());
    let dom = ocserve_store::load_sessions_wire(&db).unwrap();
    let want = serde_json::to_vec(&dom).unwrap();
    let got = ocserve_store::load_sessions_wire_bytes(&db).unwrap();
    assert_eq!(
        String::from_utf8_lossy(&got),
        String::from_utf8_lossy(&want),
        "M1 wire bytes must equal the DOM serialization byte-for-byte"
    );
    // and it must be the same length and content, not just similar text
    assert_eq!(got.len(), want.len());
    assert_eq!(&got[..], &want[..], "byte-exact");
}

#[test]
fn m1_matches_the_dom_path_on_the_real_fixture_when_available() {
    let Ok(db) = std::env::var("OCSERVE_LIST_PARITY_DB") else {
        eprintln!("skipped: OCSERVE_LIST_PARITY_DB not set");
        return;
    };
    let dom = ocserve_store::load_sessions_wire(std::path::Path::new(&db)).unwrap();
    let want = serde_json::to_vec(&dom).unwrap();
    let got = ocserve_store::load_sessions_wire_bytes(std::path::Path::new(&db)).unwrap();
    assert_eq!(
        &got[..],
        &want[..],
        "M1 must be byte-exact on the real corpus"
    );
    eprintln!("m1 parity: {} sessions, {} bytes", dom.len(), got.len());
}

/// The kill-switch must still bypass the bytes memo and produce identical
/// bytes (F5/F8 contract, and the honesty rule: cache-off must not differ).
#[test]
fn m1_kill_switch_does_not_change_the_bytes() {
    let dir = tempfile::tempdir().unwrap();
    let db = seed(dir.path());
    let with_memo = ocserve_store::load_sessions_wire_bytes(&db).unwrap();
    // SAFETY: process-global env; test is single-threaded in this binary and
    // the switch is read on every call (no caching of the flag).
    unsafe { std::env::set_var("OCSERVE_LIST_MEMO", "0") };
    let without_memo = ocserve_store::load_sessions_wire_bytes(&db).unwrap();
    unsafe { std::env::remove_var("OCSERVE_LIST_MEMO") };
    assert_eq!(&with_memo[..], &without_memo[..]);
}

/// Negative control for the *shape* claim: a member written in the wrong
/// ORDER must be detectable. This is the property the ordering depends on
/// (`serde_json` `preserve_order`), so it is asserted rather than assumed.
#[test]
fn member_order_is_the_dom_order() {
    let dir = tempfile::tempdir().unwrap();
    let db = seed(dir.path());
    let dom = ocserve_store::load_sessions_wire(&db).unwrap();
    let got = ocserve_store::load_sessions_wire_bytes(&db).unwrap();
    let text = String::from_utf8(got.to_vec()).unwrap();
    // first object's member order, read from the raw bytes
    let first: Vec<&str> = [
        "\"id\"",
        "\"projectID\"",
        "\"directory\"",
        "\"path\"",
        "\"slug\"",
        "\"title\"",
        "\"version\"",
        "\"agent\"",
        "\"model\"",
        "\"cost\"",
        "\"summary\"",
        "\"tokens\"",
        "\"time\"",
    ]
    .into_iter()
    .collect();
    let mut last = 0usize;
    for key in first {
        let at = text.find(key).unwrap_or_else(|| panic!("missing {key}"));
        assert!(at >= last, "member {key} out of order (byte {at} < {last})");
        last = at;
    }
    // and the DOM path agrees on that order — compare the whole first
    // element by slicing at the matching close bracket (the first `}` is the
    // end of the nested `summary` object, not of the session)
    let dom_first = serde_json::to_string(&dom[0]).unwrap();
    let mut depth = 0i32;
    let end = text
        .char_indices()
        .find_map(|(i, c)| match c {
            '{' => {
                depth += 1;
                None
            }
            '}' => {
                depth -= 1;
                if depth == 0 { Some(i + 1) } else { None }
            }
            _ => None,
        })
        .expect("balanced first object");
    assert_eq!(
        dom_first,
        &text[1..end],
        "member order must equal the DOM's"
    );
}

#[test]
fn writer_insert_still_round_trips_through_m1() {
    let dir = tempfile::tempdir().unwrap();
    let db = seed(dir.path());
    let writer = Writer::spawn(db.clone()).unwrap();
    let info = serde_json::json!({"id":"msg_new","role":"user"});
    ocserve_store::insert_message(&writer, None, "ses_a", &info, &[]).unwrap();
    let got = ocserve_store::load_sessions_wire_bytes(&db).unwrap();
    let text = String::from_utf8(got.to_vec()).unwrap();
    assert!(text.contains("ses_a"), "session must still be listed");
    let dom = serde_json::to_vec(&ocserve_store::load_sessions_wire(&db).unwrap()).unwrap();
    assert_eq!(
        &got[..],
        &dom[..],
        "post-write bytes must match the DOM too"
    );
}
