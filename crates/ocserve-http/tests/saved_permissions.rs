//! K-PERM-SAVED — project-level saved permissions are real state.
//!
//! The v2 routes were stubs: `GET /api/permission/saved` answered `[]` and
//! `PATCH`/`DELETE /api/permission/saved/{id}` were 204 no-ops. The shipped
//! web UI lists, edits and removes saved permissions, so a user's "always
//! (this project)" vanished the moment they left the screen — shape without
//! mutation again.
//!
//! Upstream keeps these in `PermissionTable` (permission/saved.ts): rows keyed
//! by project, unique on (project, action, resource), `add` idempotent,
//! `update` 404 on a missing id and CONFLICT on a duplicate, `remove` a bare
//! DELETE that does not 404.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use ocserve_http::AppState;
use serde_json::{Value, json};
use std::sync::Arc;
use tower::ServiceExt;

fn app() -> Arc<AppState> {
    AppState::with_payloads_and_v2(None, ocserve_http::Payloads::default(), true)
}

async fn call(
    app: &Arc<AppState>,
    method: &str,
    uri: &str,
    body: Option<Value>,
) -> (StatusCode, Value) {
    let mut b = Request::builder().method(method).uri(uri);
    let body = match body {
        Some(v) => {
            b = b.header("content-type", "application/json");
            Body::from(v.to_string())
        }
        None => Body::empty(),
    };
    let resp = ocserve_http::router(app.clone())
        .oneshot(b.body(body).unwrap())
        .await
        .unwrap();
    let st = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), 1 << 20)
        .await
        .unwrap();
    (st, serde_json::from_slice(&bytes).unwrap_or(Value::Null))
}

fn rows(app: &Arc<AppState>) -> Vec<ocserve_store::SavedPermission> {
    ocserve_store::saved_permissions(&app.db, None).unwrap()
}

#[tokio::test]
async fn a_saved_permission_is_stored_listed_edited_and_removed() {
    let app = app();
    ocserve_store::save_permission(&app.writer, "global", "bash", "git status *").unwrap();

    let (st, v) = call(&app, "GET", "/api/permission/saved", None).await;
    assert_eq!(st, StatusCode::OK);
    let list = v["data"].as_array().expect("data array");
    assert_eq!(list.len(), 1, "the saved row must be listed");
    let id = list[0]["id"].as_str().unwrap().to_string();
    assert_eq!(list[0]["projectID"], "global");
    assert_eq!(list[0]["action"], "bash");
    assert_eq!(list[0]["resource"], "git status *");
    // Schedule.Info-style contract: time is required
    assert!(list[0]["time"]["created"].as_i64().unwrap() > 0);

    // PATCH edits in place and echoes the updated row.
    let (st, v) = call(
        &app,
        "PATCH",
        &format!("/api/permission/saved/{id}"),
        Some(json!({"resource": "git push *"})),
    )
    .await;
    assert_eq!(st, StatusCode::OK, "patch failed: {v}");
    assert_eq!(v["data"]["resource"], "git push *");
    assert_eq!(rows(&app)[0].resource, "git push *");
    assert_eq!(rows(&app)[0].action, "bash", "action must be kept");

    // DELETE really removes it (204, then an empty list).
    let (st, _) = call(&app, "DELETE", &format!("/api/permission/saved/{id}"), None).await;
    assert_eq!(st, StatusCode::NO_CONTENT);
    assert!(rows(&app).is_empty(), "delete must remove the row");
}

#[tokio::test]
async fn saving_the_same_permission_twice_keeps_one_row() {
    let app = app();
    ocserve_store::save_permission(&app.writer, "global", "bash", "git status *").unwrap();
    ocserve_store::save_permission(&app.writer, "global", "bash", "git status *").unwrap();
    assert_eq!(
        rows(&app).len(),
        1,
        "upstream onConflictDoNothing semantics"
    );
}

#[tokio::test]
async fn a_duplicate_edit_is_a_conflict_and_a_missing_id_is_not_found() {
    let app = app();
    ocserve_store::save_permission(&app.writer, "global", "bash", "git status *").unwrap();
    ocserve_store::save_permission(&app.writer, "global", "edit", "src/*").unwrap();
    let a = rows(&app)
        .into_iter()
        .find(|r| r.action == "edit")
        .unwrap()
        .id;

    // Editing A to duplicate B's (action, resource) is a ConflictError.
    let (st, v) = call(
        &app,
        "PATCH",
        &format!("/api/permission/saved/{a}"),
        Some(json!({"action": "bash", "resource": "git status *"})),
    )
    .await;
    assert_eq!(st, StatusCode::CONFLICT, "expected a conflict: {v}");
    assert_eq!(v["id"], a.as_str());

    // Unknown id stays the tagged 404 (probed against 2.2.5).
    let (st, v) = call(
        &app,
        "PATCH",
        "/api/permission/saved/per_nope",
        Some(json!({"action": "allow"})),
    )
    .await;
    assert_eq!(st, StatusCode::NOT_FOUND);
    assert_eq!(v["_tag"], "PermissionSavedNotFoundError");
}

#[tokio::test]
async fn removing_an_unknown_id_is_a_no_op_like_upstream() {
    // Upstream `remove` is a bare DELETE (no existence check), so the route
    // answers 204 for an id that was never there.
    let app = app();
    let (st, _) = call(&app, "DELETE", "/api/permission/saved/per_nope", None).await;
    assert_eq!(st, StatusCode::NO_CONTENT);
}
