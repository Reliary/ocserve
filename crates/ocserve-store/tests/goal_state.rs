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

/// REGRESSION (2026-11-05 SRE review, reproduced): the goal write used to
/// compute `seq` as `MAX(seq)+1 WHERE session_id = ?` and `INSERT OR REPLACE`
/// — a per-session aggregate written into a GLOBAL primary key. Arming a goal
/// on session B could therefore REPLACE (delete) session A's event at the same
/// seq. Both goal writes now use plain INSERT with AUTOINCREMENT.
#[test]
fn goal_writes_do_not_clobber_another_sessions_events() {
    let (dir, w) = setup();
    let db = db_path(dir.path());
    w.write(vec![ocserve_store::writer::WriteOp::Sql {
        sql: "INSERT INTO session (id, time_created, time_updated) VALUES ('ses_b', 1, 1)".into(),
        params: vec![],
    }])
    .unwrap();
    // Session A's FIRST event: global seq 1 (the collision target).
    w.write(vec![ocserve_store::writer::WriteOp::Sql {
        sql: "INSERT INTO event (session_id, project_id, type, payload, time_created) \
              VALUES ('ses_g', 'global', 'session.updated@v1', '{\"a\":1}', 1)"
            .into(),
        params: vec![],
    }])
    .unwrap();
    let before: i64 = {
        let r = ocserve_store::pragma::open_reader(&db).unwrap();
        r.query_row("SELECT count(*) FROM event", [], |x| x.get(0))
            .unwrap()
    };
    // Arm goals on BOTH sessions: the OLD code computed seq=1 for each (its
    // own session had none) and REPLACE deleted A's row.
    ocserve_store::set_goal(&w, "ses_g", Some(&row("active", 0))).unwrap();
    ocserve_store::set_goal(&w, "ses_b", Some(&row("active", 0))).unwrap();
    let after: i64 = {
        let r = ocserve_store::pragma::open_reader(&db).unwrap();
        r.query_row("SELECT count(*) FROM event", [], |x| x.get(0))
            .unwrap()
    };
    assert_eq!(
        before, after,
        "arming a goal must not insert or delete ANY event row"
    );
    let a_payload: String = {
        let r = ocserve_store::pragma::open_reader(&db).unwrap();
        r.query_row(
            "SELECT payload FROM event WHERE session_id='ses_g' AND type LIKE 'session.updated%'",
            [],
            |x| x.get(0),
        )
        .unwrap()
    };
    assert_eq!(a_payload, "{\"a\":1}", "session A's event survived intact");
}
