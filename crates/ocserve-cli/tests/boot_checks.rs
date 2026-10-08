//! SRE §1 fail-fast: every boot check must FAIL when its precondition is broken
//! (forced-failure tests — a check that can't fail is theatre, TESTING §1).
//! Uses the REAL ocserve_cli::boot_checks (no mirrored logic).

use ocserve_cli::boot_checks;
use ocserve_store::{pragma, writer};

/// Happy path passes.
#[test]
fn boot_checks_pass_on_writable_dir() {
    let d = tempfile::tempdir().unwrap();
    boot_checks(d.path()).expect("healthy dir must pass boot checks");
}

/// Forced failure: read-only data dir → writable-probe check must fail loud
/// with the check named in the error.
#[cfg(unix)]
#[test]
fn boot_checks_fail_on_unwritable_dir() {
    use std::os::unix::fs::PermissionsExt;
    let d = tempfile::tempdir().unwrap();
    let sub = d.path().join("locked");
    std::fs::create_dir(&sub).unwrap();
    std::fs::set_permissions(&sub, std::fs::Permissions::from_mode(0o500)).unwrap();
    let res = boot_checks(&sub);
    std::fs::set_permissions(&sub, std::fs::Permissions::from_mode(0o700)).unwrap();
    assert!(res.is_err(), "unwritable dir must fail boot checks");
    let msg = format!("{:#}", res.unwrap_err());
    assert!(
        msg.contains("write probe"),
        "error must name the check: {msg}"
    );
}

/// Forced failure: schema version mismatch → migrate refuses (actionable msg).
#[test]
fn boot_checks_fail_on_version_mismatch() {
    let d = tempfile::tempdir().unwrap();
    boot_checks(d.path()).expect("first run ok");
    let db = writer::db_path(d.path());
    {
        let conn = pragma::open_writer(&db).unwrap();
        conn.pragma_update(None, "user_version", 999).unwrap();
    }
    let res = boot_checks(d.path());
    assert!(res.is_err(), "schema mismatch must refuse");
    let msg = format!("{:#}", res.unwrap_err());
    assert!(msg.contains("999"), "must name versions: {msg}");
}

/// Forced failure: corrupt DB file → open fails loud with context.
#[test]
fn boot_checks_fail_on_corrupt_db() {
    let d = tempfile::tempdir().unwrap();
    let db = writer::db_path(d.path());
    std::fs::write(&db, b"this is definitely not a sqlite database").unwrap();
    let res = boot_checks(d.path());
    assert!(res.is_err(), "corrupt db must fail boot checks");
    let msg = format!("{:#}", res.unwrap_err());
    assert!(!msg.is_empty(), "error must carry context");
}

/// Forced failure: a directory where the DB file should be → fails loud.
#[test]
fn boot_checks_fail_on_directory_as_db() {
    let d = tempfile::tempdir().unwrap();
    let dir_as_db = d.path().join("iamadir");
    std::fs::create_dir(&dir_as_db).unwrap();
    let res = pragma::open_writer(&dir_as_db);
    assert!(res.is_err(), "opening a directory as DB must fail");
}
