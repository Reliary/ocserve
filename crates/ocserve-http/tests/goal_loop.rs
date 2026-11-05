//! `/goal` end-to-end: the command surface, the state machine, and the loop
//! stopping rules. The reviewer call itself is exercised with a stub provider
//! so the whole turn runs for real.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use tower::ServiceExt;

fn app_st() -> (axum::Router, std::sync::Arc<ocserve_http::AppState>) {
    // PRODUCTION SHAPE: the advertised goal entry exists with `template: ""`
    // (the v1 Command schema requires the field). An empty command list would
    // make the dispatcher's predicate look different from production — the
    // exact way the 2026-11-05 `/goal` regression stayed green in tests.
    let payloads = ocserve_http::Payloads {
        command: vec![serde_json::json!({
            "name": "goal",
            "description": "keep working until an objective is met [pause|resume|clear]",
            "source": "command",
            "template": "",
            "hints": [],
        })],
        ..Default::default()
    };
    let st = ocserve_http::AppState::with_payloads_and_v2(None, payloads, true);
    (ocserve_http::router(st.clone()), st)
}

fn app() -> axum::Router {
    app_st().0
}

/// Seed an ACTIVE goal with THIS process's boot id (the state machine half;
/// the command/kickoff half is covered against a stub provider in
/// goal_e2e.rs). With no provider wired a kickoff legitimately fails and
/// pauses the goal, which is correct but obscures a pure state test.
fn seed_goal(st: &ocserve_http::AppState, sid: &str, objective: &str, id: &str) {
    ocserve_store::set_goal(
        &st.writer,
        sid,
        Some(&ocserve_store::GoalRow {
            id: id.into(),
            objective: objective.into(),
            status: "active".into(),
            reason: None,
            rounds: 0,
            max_rounds: 50,
            stalls: None,
            boot: ocserve_http::goal_loop::boot_id().to_string(),
            updated_at: 1,
        }),
    )
    .unwrap();
}

async fn req(
    app: &axum::Router,
    method: &str,
    uri: &str,
    body: Option<serde_json::Value>,
) -> axum::response::Response {
    let mut b = Request::builder().method(method).uri(uri);
    let body = match body {
        Some(v) => {
            b = b.header("content-type", "application/json");
            Body::from(serde_json::to_vec(&v).unwrap())
        }
        None => Body::empty(),
    };
    app.clone().oneshot(b.body(body).unwrap()).await.unwrap()
}

async fn json(r: axum::response::Response) -> serde_json::Value {
    let bytes = r.into_body().collect().await.unwrap().to_bytes();
    serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null)
}

async fn new_session(app: &axum::Router) -> String {
    let r = req(
        app,
        "POST",
        "/session",
        Some(serde_json::json!({"title": "t"})),
    )
    .await;
    assert_eq!(r.status(), StatusCode::OK);
    json(r).await["id"].as_str().expect("id").to_string()
}

/// `/goal pause` with no goal is the reference's own error text.
#[tokio::test]
async fn control_words_report_the_reference_messages() {
    let a = app();
    let sid = new_session(&a).await;
    for (arg, want) in [
        ("pause", "No active goal to pause."),
        ("resume", "No paused goal to resume."),
        ("clear", "No goal is set."),
    ] {
        let r = req(
            &a,
            "POST",
            &format!("/session/{sid}/command"),
            Some(serde_json::json!({"command": "goal", "arguments": arg})),
        )
        .await;
        assert_eq!(r.status(), StatusCode::BAD_REQUEST, "{arg}");
        let v = json(r).await;
        assert!(
            v["data"]["message"].as_str().unwrap().contains(want),
            "{arg}: {v}"
        );
    }
}

/// `/goal` with an empty objective shows the usage line.
#[tokio::test]
async fn empty_objective_shows_usage() {
    let a = app();
    let sid = new_session(&a).await;
    let r = req(
        &a,
        "POST",
        &format!("/session/{sid}/command"),
        Some(serde_json::json!({"command": "goal", "arguments": ""})),
    )
    .await;
    assert_eq!(r.status(), StatusCode::BAD_REQUEST);
    assert!(
        json(r).await["data"]["message"]
            .as_str()
            .unwrap()
            .contains("Usage: /goal")
    );
}

/// A 4,001-character objective is refused with the reference's wording.
#[tokio::test]
async fn objective_length_is_capped_at_4000() {
    let a = app();
    let sid = new_session(&a).await;
    let long = "x".repeat(4_001);
    let r = req(
        &a,
        "POST",
        &format!("/session/{sid}/command"),
        Some(serde_json::json!({"command": "goal", "arguments": long})),
    )
    .await;
    assert_eq!(r.status(), StatusCode::BAD_REQUEST);
    let m = json(r).await["data"]["message"]
        .as_str()
        .unwrap()
        .to_string();
    assert!(m.contains("limited to 4000 characters"), "{m}");
}

/// Setting a goal persists the state and the RPC serves it back in the
/// reference's exact shape (Goal.State or null).
#[tokio::test]
async fn goal_state_round_trips_through_the_rpc() {
    let (a, st) = app_st();
    let sid = new_session(&a).await;

    let before = req(
        &a,
        "POST",
        "/api/rpc/opencode.goal/get",
        Some(serde_json::json!({"input": {"sessionID": sid}})),
    )
    .await;
    assert_eq!(before.status(), StatusCode::OK);
    assert!(
        json(before).await["data"].is_null(),
        "no goal yet must be null, never absent"
    );

    // Seed the state directly: this test is the RPC wire shape. The
    // command->state path is covered end to end against a stub provider in
    // goal_e2e.rs.
    seed_goal(&st, &sid, "ship the release", "goal_rt");

    let after = req(
        &a,
        "POST",
        "/api/rpc/opencode.goal/get",
        Some(serde_json::json!({"input": {"sessionID": sid}})),
    )
    .await;
    let v = json(after).await;
    let goal = &v["data"];
    assert_eq!(goal["objective"], "ship the release");
    assert_eq!(goal["status"], "active");
    assert_eq!(goal["rounds"], 0);
    assert_eq!(goal["maxRounds"], 50);
    assert!(goal["boot"].as_str().is_some());
    assert!(goal["updatedAt"].is_number());
    // optional() fields are ABSENT, never null.
    assert!(goal.get("reason").is_none());
    assert!(goal.get("stalls").is_none());
}

/// Pause and resume move the state machine, and resume resets the round
/// budget (upstream: "the resume continuation is the first round of the new
/// budget").
#[tokio::test]
async fn pause_and_resume_transitions() {
    let (a, st) = app_st();
    let sid = new_session(&a).await;
    seed_goal(&st, &sid, "do the thing", "goal_pause");

    let r = req(
        &a,
        "POST",
        &format!("/session/{sid}/command"),
        Some(serde_json::json!({"command": "goal", "arguments": "pause"})),
    )
    .await;
    assert_eq!(r.status(), StatusCode::OK);
    let v = json(r).await;
    // The response is `SessionV1.WithParts`: a real AssistantMessage plus
    // parts, and the goal state alongside under `goal` (it used to be the
    // goal object AS `info`, which no strict Message parser accepts).
    assert_eq!(v["info"]["role"], "assistant");
    assert!(v["parts"].as_array().is_some_and(|p| !p.is_empty()));
    assert_eq!(v["goal"]["status"], "paused");

    let r = req(
        &a,
        "POST",
        &format!("/session/{sid}/command"),
        Some(serde_json::json!({"command": "goal", "arguments": "resume"})),
    )
    .await;
    assert_eq!(r.status(), StatusCode::OK);
    let v = json(r).await;
    assert_eq!(v["info"]["role"], "assistant");
    assert_eq!(v["goal"]["status"], "active");
    assert_eq!(v["goal"]["rounds"], 1, "resume is round 1 of a new budget");

    // clear drops it entirely
    let r = req(
        &a,
        "POST",
        &format!("/session/{sid}/command"),
        Some(serde_json::json!({"command": "goal", "arguments": "clear"})),
    )
    .await;
    assert_eq!(r.status(), StatusCode::OK);
    let after = req(
        &a,
        "POST",
        "/api/rpc/opencode.goal/get",
        Some(serde_json::json!({"input": {"sessionID": sid}})),
    )
    .await;
    assert!(json(after).await["data"].is_null());
}

/// Quoted and unquoted objectives set the same goal (upstream `unquote`).
#[tokio::test]
async fn quoted_objective_is_unquoted() {
    // Unquoting is a unit-tested pure function; here the command surface must
    // reach the state machine at all (no provider -> the kickoff pauses, but
    // the OBJECTIVE it stored is what this asserts).
    let a = app();
    let sid = new_session(&a).await;
    let _ = req(
        &a,
        "POST",
        &format!("/session/{sid}/command"),
        Some(serde_json::json!({"command": "goal", "arguments": "\"ship the release\""})),
    )
    .await;
    let r = req(
        &a,
        "POST",
        "/api/rpc/opencode.goal/get",
        Some(serde_json::json!({"input": {"sessionID": sid}})),
    )
    .await;
    assert_eq!(json(r).await["data"]["objective"], "ship the release");
}

/// The command is ADVERTISED: the runtime payload builder puts it in the
/// `/command` list (the HTTP route serves that payload verbatim). The builder
/// itself is unit-tested in `ocserve::runtime`; here we prove the route serves
/// it and that the handler accepts it.
#[tokio::test]
async fn goal_is_advertised_and_the_route_serves_it() {
    let p = ocserve_http::Payloads {
        command: vec![serde_json::json!({
            "name": "goal",
            "description": "keep working until an objective is met [pause|resume|clear]",
            "source": "command",
            "template": "",
            "hints": [],
        })],
        ..Default::default()
    };
    let a = ocserve_http::router(ocserve_http::AppState::with_payloads_and_v2(None, p, true));
    let cmds = json(req(&a, "GET", "/command", None).await).await;
    let names: Vec<&str> = cmds
        .as_array()
        .expect("command list")
        .iter()
        .filter_map(|c| c["name"].as_str())
        .collect();
    assert!(
        names.contains(&"goal"),
        "goal must be discoverable: {names:?}"
    );
}

/// A goal in another session is invisible (per-session state, like upstream's
/// per-session storage key: children and forks start without one).
#[tokio::test]
async fn goals_are_per_session() {
    let a = app();
    let s1 = new_session(&a).await;
    let s2 = new_session(&a).await;
    let _ = req(
        &a,
        "POST",
        &format!("/session/{s1}/command"),
        Some(serde_json::json!({"command": "goal", "arguments": "only for s1"})),
    )
    .await;
    let r = req(
        &a,
        "POST",
        "/api/rpc/opencode.goal/get",
        Some(serde_json::json!({"input": {"sessionID": s2}})),
    )
    .await;
    assert!(json(r).await["data"].is_null());
}

/// The v2 surface admits `/goal` too (204 + inbox), and the drain arms the
/// goal before the kickoff turn runs. Pre-fix this path bailed with
/// "command disappeared between admit and run" because the built-in entry has
/// an empty template.
#[tokio::test(flavor = "multi_thread")]
async fn v2_admission_arms_the_goal() {
    let a = app();
    let sid = new_session(&a).await;
    let r = req(
        &a,
        "POST",
        &format!("/api/session/{sid}/command"),
        Some(serde_json::json!({"name": "goal", "text": "ship it"})),
    )
    .await;
    assert_eq!(r.status(), StatusCode::NO_CONTENT);
    // The drain is spawned; poll briefly for the armed state.
    let mut armed = None;
    for _ in 0..100 {
        let g = req(
            &a,
            "POST",
            "/api/rpc/opencode.goal/get",
            Some(serde_json::json!({"input": {"sessionID": sid}})),
        )
        .await;
        let v = json(g).await;
        if !v["data"].is_null() {
            armed = Some(v["data"].clone());
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    let goal = armed.expect("v2 admission must arm the goal");
    assert_eq!(goal["objective"], "ship it");
    // No provider is wired in this fixture, so the kickoff turn itself fails
    // and the goal is honestly PAUSED (never left active with no driver).
    // The active->complete path is proven against a stub provider in
    // goal_e2e.rs; this test proves the v2 admission->drain->arm wiring.
    assert!(
        goal["status"] == "active" || goal["status"] == "paused",
        "goal armed: {goal}"
    );
}
