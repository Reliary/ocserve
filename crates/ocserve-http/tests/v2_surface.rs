//! v2 `/api/*` surface + the `GET /doc` contract route + the TUI session-diff
//! crash path (2026-10-09).
//!
//! The 2026-10-09 discovery: the 1.18.31 TUI is a hybrid v1+v2 client. Its
//! `DataProvider` and the session sidebar call `/api/*` and
//! `/session/{id}/diff`; every one of those fell through the catch-all and was
//! served HTML by the UI proxy — the same silent-failure class as the
//! /pty/shells crash. These tests assert the routes exist AND return JSON
//! (the property the earlier "20 routes, no 404s" acceptance missed, because
//! a 200 HTML body counted as success).

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use std::sync::Arc;
use tower::ServiceExt;

fn state_app() -> (Arc<ocserve_http::AppState>, axum::Router) {
    // Explicit v2=true: `AppState::new()` reads the process-global env, so
    // these tests only passed when the environment happened to set it (and a
    // gate-off run made the model-shape assertions reach the wrong parser).
    let state =
        ocserve_http::AppState::with_payloads_and_v2(None, ocserve_http::Payloads::default(), true);
    (state.clone(), ocserve_http::router(state))
}

fn app() -> axum::Router {
    state_app().1
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

async fn json(resp: axum::response::Response) -> serde_json::Value {
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null)
}

async fn new_session(app: &axum::Router) -> String {
    let r = req(
        app,
        "POST",
        "/session",
        Some(serde_json::json!({"title": "v2test"})),
    )
    .await;
    assert_eq!(r.status(), StatusCode::OK);
    json(r).await["id"].as_str().unwrap().to_string()
}

/// Every route must answer with a JSON media type — the assertion that would
/// have caught the TUI break where a 200 text/html slipped through.
async fn assert_json(app: &axum::Router, method: &str, uri: &str) -> serde_json::Value {
    let r = req(app, method, uri, None).await;
    let ct = r
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    assert_eq!(
        r.status(),
        StatusCode::OK,
        "{method} {uri} -> {}",
        r.status()
    );
    assert!(
        ct.contains("application/json"),
        "{method} {uri} served {ct}, expected JSON (the TUI break class)"
    );
    json(r).await
}

#[tokio::test]
async fn doc_route_serves_self_describing_contract() {
    let app = app();
    let v = assert_json(&app, "GET", "/doc").await;
    assert_eq!(v["openapi"], "3.1.0");
    // 162 paths in the vendored 1.18.31 contract.
    assert_eq!(v["paths"].as_object().unwrap().len(), 162);
}

#[tokio::test]
async fn session_diff_crash_path_returns_json_array() {
    let app = app();
    let sid = new_session(&app).await;
    // The proven TUI sidebar crash: this returned 200 text/html and the SDK's
    // text parse fed SidebarFiles a string it flatMapped.
    let v = assert_json(&app, "GET", &format!("/session/{sid}/diff")).await;
    assert!(v.is_array());
}

#[tokio::test]
async fn v2_permission_requests_project_v1_shape_for_reads() {
    let (st, app) = state_app();
    st.gate.register_persistent(
        "per_v1shape000000000000001",
        serde_json::json!({
            "id": "per_v1shape000000000000001",
            "sessionID": "ses_shape",
            "permission": "bash",
            "patterns": ["echo *"],
            "always": ["echo *"],
            "metadata": {"command": "echo hello"},
            "tool": {"messageID": "msg_tool", "callID": "call_tool"}
        }),
    );
    st.gate.register_persistent(
        "per_other000000000000000001",
        serde_json::json!({
            "id": "per_other000000000000000001",
            "sessionID": "ses_other",
            "permission": "read",
            "patterns": ["/tmp/file"],
            "always": [],
            "metadata": {},
            "tool": {"messageID": "msg_other", "callID": "call_other"}
        }),
    );

    let global = assert_json(&app, "GET", "/api/permission/request").await;
    let global_request = global["data"]
        .as_array()
        .unwrap()
        .iter()
        .find(|request| request["id"] == "per_v1shape000000000000001")
        .unwrap();
    assert_eq!(global_request["action"], "bash");
    assert_eq!(global_request["resources"], serde_json::json!(["echo *"]));
    assert_eq!(global_request["save"], serde_json::json!(["echo *"]));
    assert_eq!(global_request["source"]["type"], "tool");
    assert_eq!(global_request["source"]["messageID"], "msg_tool");
    assert_eq!(global_request["source"]["id"], "call_tool");
    assert!(global_request.get("permission").is_none());
    assert!(global_request.get("patterns").is_none());
    assert!(global_request.get("tool").is_none());

    let session = assert_json(&app, "GET", "/api/session/ses_shape/permission").await;
    assert_eq!(session["data"].as_array().unwrap().len(), 1);
    assert_eq!(session["data"][0], *global_request);

    let detail = assert_json(
        &app,
        "GET",
        "/api/session/ses_shape/permission/per_v1shape000000000000001",
    )
    .await;
    assert_eq!(detail["data"], *global_request);
}

/// K-V2-PERM-SHAPE: a pending ask created through the v2 route must be
/// visible to BOTH grammars in the field names each frozen contract declares,
/// and the v2 source key is `id` (2.2.5), never `callID` (1.19.5). This drives
/// the real create handler so the gate stores a v2-shaped request, then reads
/// it back through both projectors; a hand-built gate entry would not prove
/// the create path.
#[tokio::test]
async fn v2_created_ask_is_readable_in_both_grammars() {
    let (_st, app) = state_app();
    let sid = "ses_perm_both";
    let body = serde_json::json!({
        "action": "bash",
        "resources": ["echo *"],
        "save": ["echo *"],
        "metadata": {"command": "echo hi"},
        "source": {"type": "tool", "messageID": "msg_src", "id": "call_src"},
    });
    let created = assert_json_post(&app, &format!("/api/session/{sid}/permission"), body).await;
    assert_eq!(created["data"]["effect"], "ask", "{created}");
    let per_id = created["data"]["id"].as_str().expect("id").to_string();

    let v2 = assert_json(&app, "GET", &format!("/api/session/{sid}/permission")).await;
    let req = v2["data"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["id"] == per_id.as_str())
        .expect("v2 read finds the created ask");
    assert_eq!(req["action"], "bash");
    assert_eq!(req["resources"], serde_json::json!(["echo *"]));
    assert_eq!(req["source"]["id"], "call_src");
    assert!(
        req["source"].get("callID").is_none(),
        "v2 source uses id, not callID"
    );
    assert!(req.get("permission").is_none());

    let v1 = assert_json(&app, "GET", "/permission").await;
    let req1 = v1
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["id"] == per_id.as_str())
        .expect("v1 read finds the same ask");
    assert_eq!(req1["permission"], "bash");
    assert_eq!(req1["patterns"], serde_json::json!(["echo *"]));
    assert_eq!(req1["tool"]["callID"], "call_src");
    assert!(req1.get("action").is_none(), "v1 shape has no action key");
}

#[tokio::test]
async fn api_location_group_envelopes() {
    let app = app();
    for route in [
        "/api/agent",
        "/api/model",
        "/api/provider",
        "/api/command",
        "/api/skill",
        "/api/reference",
        "/api/integration",
    ] {
        let v = assert_json(&app, "GET", route).await;
        assert!(v.get("location").is_some(), "{route} missing location");
        assert!(v.get("data").is_some(), "{route} missing data");
    }
}

#[tokio::test]
async fn api_session_read_group() {
    let app = app();
    let sid = new_session(&app).await;
    assert_json(&app, "GET", "/api/session").await;
    assert_json(&app, "GET", &format!("/api/session/{sid}")).await;
    let msgs = assert_json(&app, "GET", &format!("/api/session/{sid}/message")).await;
    assert!(msgs.get("data").is_some());
}

#[tokio::test]
async fn api_session_create_shares_v1_path() {
    let app = app();
    let v = assert_json_post(&app, "/api/session", serde_json::json!({"agent": "build"})).await;
    let id = v["data"]["id"].as_str().unwrap();
    assert!(id.starts_with("ses"));
    // the created session is immediately readable via v1
    let got = assert_json(&app, "GET", &format!("/session/{id}")).await;
    assert_eq!(got["id"], id);
}

async fn assert_json_post(
    app: &axum::Router,
    uri: &str,
    body: serde_json::Value,
) -> serde_json::Value {
    let r = req(app, "POST", uri, Some(body)).await;
    assert_eq!(r.status(), StatusCode::OK, "POST {uri} -> {}", r.status());
    json(r).await
}

#[tokio::test]
async fn api_fs_find_is_location_envelope() {
    let app = app();
    let v = assert_json(&app, "GET", "/api/fs/find?query=main").await;
    assert!(v.get("location").is_some());
    assert!(v["data"].is_array());
}

#[tokio::test]
async fn v1_skill_is_bare_array() {
    let app = app();
    let v = assert_json(&app, "GET", "/skill").await;
    assert!(
        v.is_array(),
        "v1 /skill must be a bare array, not the v2 envelope"
    );
}

#[tokio::test]
async fn vcs_apply_missing_patch_payload_error() {
    let app = app();
    let r = req(&app, "POST", "/vcs/apply", Some(serde_json::json!({}))).await;
    assert_eq!(r.status(), StatusCode::BAD_REQUEST);
    let v = json(r).await;
    assert_eq!(v["name"], "BadRequest");
    assert!(v["data"]["message"].as_str().unwrap().contains("patch"));
}

#[tokio::test]
async fn share_returns_session_record() {
    let app = app();
    let sid = new_session(&app).await;
    let v = assert_json_post(
        &app,
        &format!("/session/{sid}/share"),
        serde_json::json!({}),
    )
    .await;
    assert_eq!(v["id"], sid);
    // delete mirrors the record
    let d = assert_json(&app, "DELETE", &format!("/session/{sid}/share")).await;
    assert_eq!(d["id"], sid);
}

#[tokio::test]
async fn upgrade_missing_target_payload_error() {
    let app = app();
    let r = req(&app, "POST", "/global/upgrade", Some(serde_json::json!({}))).await;
    assert_eq!(r.status(), StatusCode::BAD_REQUEST);
    let v = json(r).await;
    assert_eq!(v["name"], "BadRequest");
}

#[tokio::test]
async fn sync_start_true() {
    let app = app();
    let v = assert_json_post(&app, "/sync/start", serde_json::json!({})).await;
    assert_eq!(v, serde_json::json!(true));
}

#[tokio::test]
async fn experimental_routes_json() {
    let app = app();
    assert_json(&app, "GET", "/experimental/workspace/adapter").await;
    assert_json(&app, "GET", "/experimental/worktree").await;
    assert_json(&app, "GET", "/experimental/console/orgs").await;
    assert_json(&app, "GET", "/api/permission/saved").await;
    assert_json(&app, "GET", "/api/question/request").await;
}

/// Content-type is the invariant that distinguishes a real route from the SPA
/// fallback. Assert it explicitly for the API prefix even where the body is
/// empty/err, so a future route removal cannot silently re-introduce HTML.
#[tokio::test]
async fn api_prefix_never_serves_html() {
    let app = app();
    // A known-bound route and an unknown one under /api/ must both be JSON-ish
    // (unknown falls through to the app HTML by design — that is the catch-all
    // parity). We only assert the *bound* set here.
    let sid = new_session(&app).await;
    for uri in [
        format!("/api/session/{sid}"),
        "/api/agent".to_string(),
        "/api/permission/saved".to_string(),
    ] {
        let r = req(&app, "GET", &uri, None).await;
        let ct = r
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");
        assert!(!ct.contains("text/html"), "{uri} served HTML: {ct}");
    }
}

/// P0 (2026-10-09): the v2 permission-create route IS the permission oracle —
/// it returns the evaluated effect, never a hardcoded allow. A fixture state
/// with an `edit: deny` agent must yield deny; a default (`*: allow`) agent
/// yields allow. This is the regression test for the hardcoded-allow bug.
#[tokio::test]
async fn permission_create_evaluates_ruleset() {
    use ocserve_http::{AppState, LlmRegistry, Payloads, Wires};
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("p.db");
    let blobs =
        std::sync::Arc::new(ocserve_store::BlobStore::new(dir.path().join("blobs")).unwrap());
    let writer = std::sync::Arc::new(ocserve_store::Writer::spawn(db.clone()).unwrap());
    let llm = LlmRegistry {
        modalities: Default::default(),
        endpoints: Default::default(),
        pricing: Default::default(),
        limits: Default::default(),
        default_model: ("p".into(), "m".into()),
        systems: Default::default(),
        default_agent: "build".into(),
    };
    // agent "build" allows all; agent "locked" denies edit.
    let payloads = Payloads {
        agent: vec![
            serde_json::json!({"name":"build","permission":[{"permission":"*","pattern":"*","action":"allow"}]}),
            serde_json::json!({"name":"locked","permission":[{"permission":"edit","pattern":"*","action":"deny"}]}),
            serde_json::json!({"name":"asky","permission":[{"permission":"webfetch","pattern":"*","action":"ask"}]}),
        ],
        ..Payloads::default()
    };
    let st = AppState::with_wiring(
        None,
        payloads,
        Wires {
            db,
            blobs,
            writer,
            llm,
        },
    );
    std::mem::forget(dir);
    let app = ocserve_http::router(st.clone());

    // create a session
    let r = req(&app, "POST", "/session", Some(serde_json::json!({}))).await;
    let sid = json(r).await["id"].as_str().unwrap().to_string();

    // default agent (build) → allow
    let r = req(
        &app,
        "POST",
        &format!("/api/session/{sid}/permission"),
        Some(serde_json::json!({"action":"read","resources":["src/x.rs"],"save":[]})),
    )
    .await;
    assert_eq!(r.status(), StatusCode::OK);
    assert_eq!(json(r).await["data"]["effect"], "allow");

    // explicit agent "locked" → deny (the exact hardcoded-allow failure)
    let r = req(
        &app,
        "POST",
        &format!("/api/session/{sid}/permission"),
        Some(serde_json::json!({"action":"edit","resources":["/etc/passwd"],"save":[],"agent":"locked"})),
    )
    .await;
    assert_eq!(r.status(), StatusCode::OK);
    assert_eq!(
        json(r).await["data"]["effect"],
        "deny",
        "a deny rule must yield deny, not the old hardcoded allow"
    );

    // Ask stores v2 grammar in the shared gate; both read boundaries project
    // to their frozen schemas, including the v1 UI's strict additionalProperties.
    let response = req(
        &app,
        "POST",
        &format!("/api/session/{sid}/permission"),
        Some(serde_json::json!({
            "action":"webfetch",
            "resources":["https://example.test"],
            "save":["https://example.test/*"],
            "metadata":{"url":"https://example.test"},
            "source":{"type":"tool","messageID":"msg_1","id":"call_1"},
            "agent":"asky"
        })),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let permission_id = json(response).await["data"]["id"]
        .as_str()
        .unwrap()
        .to_string();

    let v2 = assert_json(&app, "GET", "/api/permission/request").await;
    let v2_request = v2["data"]
        .as_array()
        .unwrap()
        .iter()
        .find(|request| request["id"] == permission_id)
        .unwrap();
    assert_eq!(v2_request["action"], "webfetch");
    assert_eq!(
        v2_request["resources"],
        serde_json::json!(["https://example.test"])
    );
    assert_eq!(
        v2_request["save"],
        serde_json::json!(["https://example.test/*"])
    );

    let v1 = assert_json(&app, "GET", "/permission").await;
    let v1_request = v1
        .as_array()
        .unwrap()
        .iter()
        .find(|request| request["id"] == permission_id)
        .unwrap();
    assert_eq!(v1_request["permission"], "webfetch");
    assert_eq!(
        v1_request["patterns"],
        serde_json::json!(["https://example.test"])
    );
    assert_eq!(
        v1_request["always"],
        serde_json::json!(["https://example.test/*"])
    );
    assert_eq!(v1_request["tool"]["messageID"], "msg_1");
    assert_eq!(v1_request["tool"]["callID"], "call_1");
    for v2_only in ["action", "resources", "save", "source"] {
        assert!(v1_request.get(v2_only).is_none(), "v1 leaked {v2_only}");
    }

    // V2 requests retain their wire grammar in the shared gate, but `always`
    // must still create the grant and cascade to matching pending asks.
    let sibling_response = req(
        &app,
        "POST",
        &format!("/api/session/{sid}/permission"),
        Some(serde_json::json!({
            "action":"webfetch",
            "resources":["https://example.test/page"],
            "save":["https://example.test/*"],
            "agent":"asky"
        })),
    )
    .await;
    assert_eq!(sibling_response.status(), StatusCode::OK);
    let sibling_id = json(sibling_response).await["data"]["id"]
        .as_str()
        .unwrap()
        .to_string();

    let reply = req(
        &app,
        "POST",
        &format!("/api/session/{sid}/permission/{permission_id}/reply"),
        Some(serde_json::json!({"reply":"always"})),
    )
    .await;
    assert_eq!(reply.status(), StatusCode::OK);
    assert!(
        st.gate
            .check_always(&sid, "webfetch", "https://example.test/page")
    );
    assert!(
        !st.gate
            .list()
            .iter()
            .any(|request| request["id"] == sibling_id),
        "the matching v2 request should be resolved by the always grant"
    );
}

#[tokio::test]
async fn permission_create_rejects_malformed_request_fields_before_persisting() {
    use ocserve_http::{AppState, LlmRegistry, Payloads, Wires};
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("p.db");
    let blobs =
        std::sync::Arc::new(ocserve_store::BlobStore::new(dir.path().join("blobs")).unwrap());
    let writer = std::sync::Arc::new(ocserve_store::Writer::spawn(db.clone()).unwrap());
    let llm = LlmRegistry {
        modalities: Default::default(),
        endpoints: Default::default(),
        pricing: Default::default(),
        limits: Default::default(),
        default_model: ("p".into(), "m".into()),
        systems: Default::default(),
        default_agent: "build".into(),
    };
    let payloads = Payloads {
        agent: vec![serde_json::json!({
            "name":"asky",
            "permission":[{"permission":"webfetch","pattern":"*","action":"ask"}]
        })],
        ..Payloads::default()
    };
    let st = AppState::with_wiring(
        None,
        payloads,
        Wires {
            db,
            blobs,
            writer,
            llm,
        },
    );
    std::mem::forget(dir);
    let app = ocserve_http::router(st);
    let sid = new_session(&app).await;
    let cases = [
        serde_json::json!({"action":"webfetch","resources":[7],"agent":"asky"}),
        serde_json::json!({"action":"webfetch","resources":["https://example.test"],"save":[false],"agent":"asky"}),
        serde_json::json!({"action":"webfetch","resources":["https://example.test"],"save":null,"agent":"asky"}),
        serde_json::json!({"action":"webfetch","resources":["https://example.test"],"agent":7}),
        serde_json::json!({"action":"webfetch","resources":["https://example.test"],"metadata":null,"agent":"asky"}),
        serde_json::json!({"action":"webfetch","resources":["https://example.test"],"source":{"type":"user","messageID":"msg_1","id":"call_1"},"agent":"asky"}),
        serde_json::json!({"action":"webfetch","resources":["https://example.test"],"source":{"type":"tool","messageID":"msg_1"},"agent":"asky"}),
        serde_json::json!({"action":"webfetch","resources":["https://example.test"],"source":{"type":"tool","messageID":"msg_1","id":"call_1","extra":true},"agent":"asky"}),
        serde_json::json!({"action":"webfetch","resources":["https://example.test"],"message":3,"agent":"asky"}),
        serde_json::json!({"action":"webfetch","resources":["https://example.test"],"message":"extra field","agent":"asky"}),
    ];
    let mut accepted_invalid_cases = Vec::new();
    for (index, body) in cases.into_iter().enumerate() {
        let response = req(
            &app,
            "POST",
            &format!("/api/session/{sid}/permission"),
            Some(body),
        )
        .await;
        let status = response.status();
        let value = json(response).await;
        if status != StatusCode::BAD_REQUEST {
            accepted_invalid_cases.push((index, status, value["_tag"].clone()));
        } else {
            assert_eq!(value["_tag"], "InvalidRequestError");
        }
    }
    assert!(
        accepted_invalid_cases.is_empty(),
        "schema-invalid cases were accepted: {accepted_invalid_cases:?}"
    );
    let pending = assert_json(&app, "GET", "/api/permission/request").await;
    assert_eq!(pending["data"], serde_json::json!([]));
    // Negative control: without entry/value validation the malformed resource
    // is skipped and answered allow; malformed optional fields can be stored
    // by an ask rule and later violate Permission.Request's frozen schema.
}

/// 2026-10-11 bug hunt: v2 write routes must 404 an unknown session with the
/// tagged SessionNotFoundError (freeze), not 204 — `set_session_field` ran the
/// UPDATE blind. And v2 list cursors must carry BOTH keys (previous/next, null
/// when absent) like freeze; ocserve emitted `{}`.
#[tokio::test]
async fn v2_unknown_session_write_404s_and_cursors_are_shaped() {
    let app = app();
    let bogus = "ses_00000000000000000000000000";
    let r = req(
        &app,
        "POST",
        &format!("/api/session/{bogus}/agent"),
        Some(serde_json::json!({"agent": "build"})),
    )
    .await;
    assert_eq!(r.status(), StatusCode::NOT_FOUND);
    let j = json(r).await;
    assert_eq!(j["_tag"], "SessionNotFoundError");
    assert_eq!(j["sessionID"], bogus);

    let r = req(
        &app,
        "POST",
        &format!("/api/session/{bogus}/model"),
        Some(serde_json::json!({"model":{"id":"x","providerID":"y"}})),
    )
    .await;
    assert_eq!(r.status(), StatusCode::NOT_FOUND);

    // list cursor shape: both keys present (nulls when no pagination)
    let r = req(&app, "GET", "/api/session?limit=2&order=desc", None).await;
    let j = json(r).await;
    assert!(
        j["cursor"].get("previous").is_some(),
        "cursor.previous key: {j}"
    );
    assert!(j["cursor"].get("next").is_some(), "cursor.next key: {j}");
}
