//! W4 `PATCH /config` + `/global/config` — source-derived contract
//! (v1 handlers/config.ts: echo payload, merge, disposal analog = reloader).

use axum::body::Body;
use axum::http::Request;
use http_body_util::BodyExt;
use ocserve_http::{AppState, LlmRegistry, Payloads, Wires};
use tower::ServiceExt;

fn state(dir: &std::path::Path) -> std::sync::Arc<AppState> {
    let db = ocserve_store::writer::db_path(dir);
    let writer = std::sync::Arc::new(ocserve_store::Writer::spawn(db.clone()).unwrap());
    let blobs = std::sync::Arc::new(ocserve_store::BlobStore::new(dir.join("blobs")).unwrap());
    let llm = LlmRegistry {
        limits: std::collections::HashMap::new(),
        endpoints: [("fake".into(), ("http://127.0.0.1:9".into(), String::new()))]
            .into_iter()
            .collect(),
        pricing: Default::default(),
        default_model: ("fake".into(), "m".into()),
        systems: Default::default(),
        default_agent: "build".into(),
    };
    AppState::with_wiring(
        None,
        Payloads::default(),
        Wires {
            db,
            blobs,
            writer,
            llm,
        },
    )
}

#[test]
fn deep_merge_rules() {
    let mut base = serde_json::json!({"a": {"b": 1, "c": 2}, "arr": [1, 2], "keep": true});
    ocserve_http::deep_merge(
        &mut base,
        serde_json::json!({"a": {"b": 9, "d": 4}, "arr": [3], "new": "x"}),
    );
    assert_eq!(base["a"]["b"], 9, "nested overwrite");
    assert_eq!(base["a"]["c"], 2, "nested sibling kept");
    assert_eq!(base["a"]["d"], 4, "nested insert");
    assert_eq!(
        base["arr"],
        serde_json::json!([3]),
        "arrays REPLACE (not concat)"
    );
    assert_eq!(base["keep"], true, "untouched");
    assert_eq!(base["new"], "x", "new key inserted");
}

#[tokio::test]
async fn patch_merges_writes_atomically_and_reloads() {
    // isolate: HOME → temp (overlay path = <tmp>/.config/ocserve/config.json)
    let home = tempfile::tempdir().unwrap();
    // SAFETY: own test binary, single test — no concurrent env readers
    unsafe { std::env::set_var("HOME", home.path()) };
    unsafe { std::env::set_var("OCSERVE_CONFIG_WRITE", "overlay") };

    let dir = tempfile::tempdir().unwrap();
    let st = state(dir.path());
    // boot-injected reloader analog: serve the merged FILE content as config
    let cfg_path = home.path().join(".config/ocserve/config.json");
    let cfg_path2 = cfg_path.clone();
    let reloader: std::sync::Arc<
        dyn Fn() -> anyhow::Result<(Payloads, ocserve_http::LlmRegistry)> + Send + Sync,
    > = std::sync::Arc::new(move || {
        let mut p = Payloads::default();
        if cfg_path2.exists() {
            let raw = std::fs::read_to_string(&cfg_path2)?;
            p.config = serde_json::from_str(&raw)?;
        }
        Ok((p, ocserve_http::LlmRegistry::default()))
    });
    *st.reloader.write() = Some(reloader);
    let app = ocserve_http::router(st.clone());

    let patch = |body: serde_json::Value| {
        let app = app.clone();
        async move {
            app.oneshot(
                Request::builder()
                    .method("PATCH")
                    .uri("/config")
                    .header("content-type", "application/json")
                    .body(Body::from(body.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap()
        }
    };

    // first PATCH: echo body byte-shaped, file created, GET reflects (reload)
    let resp = patch(serde_json::json!({"model": {"primary": "deepseek/deepseek-v4-pro"}})).await;
    assert_eq!(resp.status(), 200);
    let echo = resp.into_body().collect().await.unwrap().to_bytes();
    assert_eq!(
        std::str::from_utf8(&echo).unwrap(),
        r#"{"model":{"primary":"deepseek/deepseek-v4-pro"}}"#,
        "v1 update returns the ECHOED payload (config.ts:21)"
    );
    assert!(cfg_path.exists(), "overlay file written");
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/config")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let cfg: serde_json::Value =
        serde_json::from_slice(&resp.into_body().collect().await.unwrap().to_bytes()).unwrap();
    assert_eq!(
        cfg["model"]["primary"], "deepseek/deepseek-v4-pro",
        "GET reflects after reload"
    );

    // second PATCH: merge + .bak kept (atomic swap)
    let resp = patch(serde_json::json!({"theme": "dark"})).await;
    assert_eq!(resp.status(), 200);
    let raw = std::fs::read_to_string(&cfg_path).unwrap();
    let v: serde_json::Value = serde_json::from_str(&raw).unwrap();
    assert_eq!(v["theme"], "dark");
    assert_eq!(
        v["model"]["primary"], "deepseek/deepseek-v4-pro",
        "merged, not replaced"
    );
    assert!(
        home.path()
            .join(".config/ocserve/.opencode.json.bak")
            .exists(),
        ".bak kept"
    );

    // /global/config shares the handler (single-config divergence, TESTING §1.6)
    let resp = app
        .clone()
        .oneshot(
            Request::builder()
                .method("PATCH")
                .uri("/global/config")
                .header("content-type", "application/json")
                .body(Body::from(r#"{"editor":{"fontSize":14}}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);

    // no reloader → still writes + echoes (warn path), no panic
    let dir2 = tempfile::tempdir().unwrap();
    let st2 = state(dir2.path());
    let app2 = ocserve_http::router(st2);
    let resp = app2
        .oneshot(
            Request::builder()
                .method("PATCH")
                .uri("/config")
                .header("content-type", "application/json")
                .body(Body::from(r#"{"x":1}"#))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), 200, "reloader-optional (tests/boot)");
}
