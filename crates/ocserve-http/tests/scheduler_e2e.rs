//! K-SCHEDULE — a scheduled wake-up actually fires.
//!
//! The v2 `experimental.schedule.*` routes shipped as storage only: a schedule
//! was created, listed, paused and resumed, and **never ran**. The web UI
//! creates these (12 references in the shipped 2.2.5 bundle), so a user set a
//! wake-up and nothing happened, with no error and no event — shape without
//! mutation, the class this repo rejects.
//!
//! These tests drive the real runner against a real store. A schedule is due
//! *now*, `scheduler::pass` runs, and the assertions are about durable rows:
//! an inbox item admitted with the deterministic id, the schedule's own state
//! transition, and the "does not fire twice" property on a second pass.
//!
//! Every test pins its own data dir: the runner reads and writes real state, so
//! a shared temp dir would couple the cases (the parallel-tempdir bug class).

use axum::body::Body;
use axum::http::{Request, StatusCode};
use ocserve_http::AppState;
use serde_json::{Value, json};
use std::sync::Arc;
use tower::ServiceExt;

fn app() -> Arc<AppState> {
    AppState::with_payloads_and_v2(
        None,
        ocserve_http::Payloads {
            config: json!({"experimental": {"schedules": true}}),
            ..Default::default()
        },
        true,
    )
}

async fn post(app: &Arc<AppState>, uri: &str, body: Value) -> (StatusCode, Value) {
    let resp = ocserve_http::router(app.clone())
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(uri)
                .header("content-type", "application/json")
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), 1 << 20)
        .await
        .unwrap();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

async fn session(app: &Arc<AppState>) -> String {
    let (st, v) = post(app, "/session", json!({"title": "sched"})).await;
    assert_eq!(st, StatusCode::OK);
    v["id"].as_str().expect("session id").to_string()
}

/// The `when` input as upstream accepts it: UNTAGGED, selected by key.
async fn create(app: &Arc<AppState>, sid: &str, when: Value, extra: Value) -> Value {
    let mut body = json!({"sessionID": sid, "prompt": "WAKE", "when": when});
    if let Some(o) = extra.as_object() {
        for (k, v) in o {
            body[k] = v.clone();
        }
    }
    let (st, v) = post(app, "/api/experimental/schedule", body).await;
    assert_eq!(st, StatusCode::OK, "create failed: {v}");
    v["data"].clone()
}

/// Run one runner pass and return how long it asked to sleep.
async fn pass(app: &Arc<AppState>) -> i64 {
    ocserve_http::scheduler::pass_for_test(app).await
}

fn inbox_len(app: &Arc<AppState>, sid: &str) -> i64 {
    ocserve_store::inbox::pending_count(&app.db, sid).unwrap_or(-1)
}

#[tokio::test]
async fn a_due_schedule_admits_a_prompt_and_marks_the_run() {
    let app = app();
    let sid = session(&app).await;
    // `in: 1 second` is due almost immediately; drive the clock with a tiny
    // wait rather than mocking time (the runner reads the wall clock once).
    let s = create(&app, &sid, json!({"in": "1 second"}), json!({})).await;
    let id = s["id"].as_str().unwrap().to_string();
    assert_eq!(s["status"], "active");
    assert_eq!(s["runCount"], 0);

    // Before it is due, a pass must do nothing.
    pass(&app).await;
    assert_eq!(inbox_len(&app, &sid), 0, "must not fire early");

    tokio::time::sleep(std::time::Duration::from_millis(1100)).await;
    pass(&app).await;

    assert_eq!(inbox_len(&app, &sid), 1, "the wake must be admitted");
    let (_, _, text, _, _) = ocserve_store::inbox::get(&app.db, &format!("msg_{id}_1"))
        .unwrap()
        .expect("deterministic message id msg_<id>_1");
    let payload: Value = serde_json::from_str(&text).unwrap();
    assert_eq!(payload["text"], "WAKE");
    assert_eq!(payload["metadata"]["schedule"]["id"], id.as_str());
    assert_eq!(payload["metadata"]["schedule"]["run"], 1);

    // The row recorded the firing.
    let (_, listed) = {
        let r = ocserve_http::router(app.clone())
            .oneshot(
                Request::builder()
                    .uri(format!("/api/experimental/schedule?sessionID={sid}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let b = axum::body::to_bytes(r.into_body(), 1 << 20).await.unwrap();
        (StatusCode::OK, serde_json::from_slice::<Value>(&b).unwrap())
    };
    let row = &listed["data"][0];
    assert_eq!(row["runCount"], 1);
    assert_eq!(row["status"], "done", "an `in` schedule is a one-shot");
    assert!(row["nextDueAt"].is_null(), "no next slot after a one-shot");
    // Caught by the live smoke: `Schedule.Info.time` is REQUIRED by the
    // contract, and a listing without it shows 1970 in a generated client.
    assert!(
        row["time"]["created"].as_i64().unwrap_or(0) > 0,
        "time.created must be a real millis value: {row}"
    );
    assert!(row["time"]["updated"].as_i64().unwrap_or(0) > 0);
    // The firing state the contract declares is emitted, not just stored.
    assert!(row["lastFiredAt"].as_i64().unwrap_or(0) > 0, "lastFiredAt");
    assert_eq!(row["lastOutcome"], "fired");

    // A second pass must not re-admit: the row is done.
    pass(&app).await;
    assert_eq!(inbox_len(&app, &sid), 1, "must not fire twice");
}

#[tokio::test]
async fn an_every_schedule_advances_instead_of_repeating() {
    let app = app();
    let sid = session(&app).await;
    let s = create(
        &app,
        &sid,
        json!({"every": "1 second"}),
        json!({"ends": {"times": 2}}),
    )
    .await;
    let id = s["id"].as_str().unwrap().to_string();

    tokio::time::sleep(std::time::Duration::from_millis(1100)).await;
    pass(&app).await;
    // Deliver the admitted item so the next slot is not "still pending".
    ocserve_store::inbox::get(&app.db, &format!("msg_{id}_1"))
        .unwrap()
        .expect("first run admitted");

    // Move the due time into the past again: the runner must schedule the NEXT
    // slot (phase preserved), not re-fire the same one.
    ocserve_http::scheduler::force_due_for_test(&app, &id, 0).await;

    tokio::time::sleep(std::time::Duration::from_millis(1100)).await;
    pass(&app).await;
    let (_, _, text, _, _) = ocserve_store::inbox::get(&app.db, &format!("msg_{id}_2"))
        .unwrap()
        .expect("second run uses run=2 in the id");
    let payload: Value = serde_json::from_str(&text).unwrap();
    assert_eq!(payload["metadata"]["schedule"]["run"], 2);
}

#[tokio::test]
async fn a_schedule_does_not_fire_when_the_feature_flag_is_off() {
    let app = AppState::with_payloads_and_v2(None, ocserve_http::Payloads::default(), true);
    let sid = session(&app).await;
    // Create with the flag OFF: the route itself refuses, so seed the row the
    // way the runner sees it (a database written by an earlier, enabled boot).
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64;
    app.writer
        .write(vec![ocserve_store::writer::WriteOp::Sql {
            sql: "INSERT INTO schedule \
                  (id, session_id, name, prompt, when_json, tz, status, next_due_ms, \
                   run_count, time_created, time_updated) \
                  VALUES ('sch_off', ?1, '', 'WAKE', ?2, '', 'active', ?3, 0, ?4, ?4)"
                .to_string(),
            params: vec![
                json!(sid),
                json!(r#"{"type":"in","in":"1 second"}"#),
                json!(now - 1000),
                json!(now),
            ],
        }])
        .unwrap();

    pass(&app).await;
    assert_eq!(
        inbox_len(&app, &sid),
        0,
        "schedules must not fire while the flag is off"
    );
}

#[tokio::test]
async fn a_paused_schedule_never_fires() {
    let app = app();
    let sid = session(&app).await;
    let s = create(&app, &sid, json!({"in": "1 second"}), json!({})).await;
    let id = s["id"].as_str().unwrap().to_string();
    let (st, _) = post(
        &app,
        &format!("/api/experimental/schedule/{id}/pause"),
        json!({}),
    )
    .await;
    assert_eq!(st, StatusCode::OK);

    tokio::time::sleep(std::time::Duration::from_millis(1100)).await;
    pass(&app).await;
    assert_eq!(inbox_len(&app, &sid), 0, "a paused row must not fire");
}
