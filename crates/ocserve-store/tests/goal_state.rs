//! `/goal` state persistence (schema v17): one row per session, cleared on
//! delete, and the boot scan only ever sees ACTIVE goals.

use ocserve_store::writer::db_path;

fn setup() -> (tempfile::TempDir, ocserve_store::Writer) {
    let dir = tempfile::tempdir().unwrap();
    let w = ocserve_store::Writer::spawn(db_path(dir.path())).unwrap();
    // FK parent.
    w.write(vec![ocserve_store::writer::WriteOp::Sql {
        sql: "INSERT INTO session (id, time_created, time_updated) VALUES ('ses_g', 1, 1)".into(),
        params: vec![],
    }])
    .unwrap();
    (dir, w)
}

fn row(status: &str, rounds: i64) -> ocserve_store::GoalRow {
    ocserve_store::GoalRow {
        id: "goal_1".into(),
        objective: "ship the thing".into(),
        status: status.into(),
        reason: Some("because".into()),
        rounds,
        max_rounds: 50,
        stalls: None,
        boot: "boot-1".into(),
        updated_at: 5,
    }
}

#[test]
fn goal_round_trips_and_clears() {
    let (dir, w) = setup();
    let db = db_path(dir.path());
    assert!(ocserve_store::get_goal(&db, "ses_g").unwrap().is_none());

    ocserve_store::set_goal(&w, "ses_g", Some(&row("active", 0))).unwrap();
    let g = ocserve_store::get_goal(&db, "ses_g").unwrap().expect("row");
    assert_eq!(g.objective, "ship the thing");
    assert_eq!(g.status, "active");
    // The wire shape uses upstream's camelCase keys and OMITS absent options.
    let j = g.to_json();
    assert_eq!(j["maxRounds"], 50);
    assert!(j.get("stalls").is_none());
    assert!(j.get("reason").is_some());

    ocserve_store::set_goal(&w, "ses_g", Some(&row("paused", 3))).unwrap();
    let g = ocserve_store::get_goal(&db, "ses_g").unwrap().unwrap();
    assert_eq!((g.status.as_str(), g.rounds), ("paused", 3));

    ocserve_store::set_goal(&w, "ses_g", None).unwrap();
    assert!(ocserve_store::get_goal(&db, "ses_g").unwrap().is_none());
}

#[test]
fn boot_scan_sees_only_active_goals() {
    let (dir, w) = setup();
    let db = db_path(dir.path());
    ocserve_store::set_goal(&w, "ses_g", Some(&row("active", 1))).unwrap();
    // A second session with a paused goal must NOT be resumed at boot.
    w.write(vec![ocserve_store::writer::WriteOp::Sql {
        sql: "INSERT INTO session (id, time_created, time_updated) VALUES ('ses_p', 1, 1)".into(),
        params: vec![],
    }])
    .unwrap();
    ocserve_store::set_goal(&w, "ses_p", Some(&row("paused", 1))).unwrap();

    let active = ocserve_store::active_goals(&db).unwrap();
    assert_eq!(active.len(), 1);
    assert_eq!(active[0].0, "ses_g");
}

#[test]
fn deleting_a_session_drops_its_goal() {
    let (dir, w) = setup();
    let db = db_path(dir.path());
    ocserve_store::set_goal(&w, "ses_g", Some(&row("active", 0))).unwrap();
    w.write(vec![ocserve_store::writer::WriteOp::Sql {
        sql: "DELETE FROM session WHERE id = 'ses_g'".into(),
        params: vec![],
    }])
    .unwrap();
    assert!(
        ocserve_store::get_goal(&db, "ses_g").unwrap().is_none(),
        "FK cascade must drop the goal with the session"
    );
}
