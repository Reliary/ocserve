//! K-V2-TRANSFORM: a v2 plugin's `command.transform` / `skill.transform` edits
//! reach BOTH consumers of `Payloads.command` — the `GET /command` route AND
//! the system prompt's `<available_skills>` block that the model reads.
//!
//! This is the all-consumers rule (TESTING §1) made executable. A test that
//! only asserted the plugin round-trip would pass while the prompt kept the
//! pre-transform text — shape without mutation. Here the transform rewrites a
//! skill description, and we assert the description the MODEL receives changed.
//!
//! Second assertion: a throwing transform keeps the pristine list (a broken
//! third-party plugin must not empty the server's command surface), and the
//! `<available_skills>` block still renders.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use ocserve_core::system_prompt;
use ocserve_http::watch::reconcile;
use ocserve_http::{AppState, ConfigReloader, LlmRegistry, Payloads, Wires};
use serde_json::{Value, json};
use std::sync::Arc;
use tower::ServiceExt;

/// A payload set with ONE skill (the vector `skill_entries` filters on
/// `source=="skill"`) and one config command.
fn payloads() -> Payloads {
    Payloads {
        command: vec![
            json!({"name": "demo-skill", "description": "ORIGINAL-DESCRIPTION", "source": "skill",
                   "location": "/skills/demo", "template": "body"}),
            json!({"name": "review", "description": "config command", "source": "config",
                   "template": "review $ARGUMENTS"}),
        ],
        ..Default::default()
    }
}

fn registry() -> LlmRegistry {
    LlmRegistry {
        limits: std::collections::HashMap::new(),
        endpoints: [("fake".into(), ("http://127.0.0.1:9".into(), String::new()))]
            .into_iter()
            .collect(),
        pricing: Default::default(),
        default_model: ("fake".into(), "m".into()),
        systems: Default::default(),
        default_agent: "build".into(),
    }
}

async fn state(dir: &std::path::Path) -> Arc<AppState> {
    let db = ocserve_store::writer::db_path(dir);
    let writer = Arc::new(ocserve_store::Writer::spawn(db.clone()).unwrap());
    let blobs = Arc::new(ocserve_store::BlobStore::new(dir.join("blobs")).unwrap());
    AppState::with_wiring(
        None,
        payloads(),
        Wires {
            db,
            blobs,
            writer,
            llm: registry(),
        },
    )
}

/// Boot a state whose reloader always hands back the pristine command list,
/// with a real plugin sidecar carrying a transform fixture.
async fn with_transform_plugin(
    dir: &std::path::Path,
    plugin_src: &str,
) -> anyhow::Result<Arc<AppState>> {
    let st = state(dir).await;
    let reload: ConfigReloader = Arc::new(move || Ok((payloads(), registry())));
    *st.reloader.write() = Some(reload);

    let plugin_dir = dir.join("plugin-src");
    std::fs::create_dir_all(&plugin_dir).unwrap();
    let entry = plugin_dir.join("p.mjs");
    std::fs::write(&entry, plugin_src).unwrap();
    let host = ocserve_plugin::materialize_host(&dir.join("plugin-host"))?;
    let mut sc =
        ocserve_plugin::Sidecar::spawn(&host, "http://127.0.0.1:1", dir.to_string_lossy().as_ref())
            .await?;
    sc.load("p", &entry, &json!({"directory": dir}), &Value::Null)
        .await?;
    st.plugins.set(Arc::new(tokio::sync::Mutex::new(sc))).ok();
    Ok(st)
}

/// The description the MODEL sees: the PRODUCTION `skill_entries` → `skills_block`.
/// Deliberately calls the real function — a re-implementation of its
/// `source == "skill"` filter proved nothing (the planted break stayed green).
fn prompt_skill_description(st: &Arc<AppState>) -> String {
    let rows: Vec<(String, String, String)> = ocserve_http::skill_entries(st)
        .into_iter()
        .map(|(n, d, l, _)| (n, d, l))
        .collect();
    system_prompt::skills_block(&rows).unwrap_or_default()
}

async fn get_commands(st: &Arc<AppState>) -> Vec<Value> {
    let resp = ocserve_http::router(st.clone())
        .oneshot(
            Request::builder()
                .uri("/command")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let b = resp.into_body().collect().await.unwrap().to_bytes();
    serde_json::from_slice(&b).unwrap()
}

/// The whole point: a skill transform must change what the model is told.
#[tokio::test]
async fn v2_skill_transform_changes_the_model_facing_prompt() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let st = with_transform_plugin(
        dir.path(),
        r#"export default {
  id: "tx",
  setup: async (ctx) => {
    await ctx.skill.transform((editor) => {
      editor.update("demo-skill", (s) => { s.description = "PLUGIN-REWRITTEN"; });
    });
  },
};"#,
    )
    .await?;

    // Before reconcile the served list is pristine.
    let before = prompt_skill_description(&st);
    assert!(before.contains("ORIGINAL-DESCRIPTION"), "{before}");

    reconcile(&st).await?;

    // 1. the SYSTEM PROMPT the model reads now carries the plugin's edit —
    //    this is the assertion a route-only test would silently miss.
    let after = prompt_skill_description(&st);
    assert!(
        after.contains("PLUGIN-REWRITTEN") && !after.contains("ORIGINAL-DESCRIPTION"),
        "skill.transform must reach the prompt's <available_skills>, got: {after}"
    );

    // 2. and the same edit is on the served route (all consumers, one vector).
    let cmds = get_commands(&st).await;
    assert!(
        cmds.iter()
            .any(|c| c["name"] == "demo-skill" && c["description"] == "PLUGIN-REWRITTEN"),
        "GET /command must serve the transformed skill too: {cmds:?}"
    );
    // untouched entries survive (the rule cuts both ways)
    assert!(cmds.iter().any(|c| c["name"] == "review"));

    // 3. the prompt carries ONLY skills — a plain command must not leak into
    //    `<available_skills>`. Without this the test still passed when
    //    skill_entries' `source == "skill"` filter was broken (planted
    //    control), i.e. it was not really exercising the filter.
    assert!(
        !after.contains("<name>review</name>"),
        "a non-skill command must not reach <available_skills>: {after}"
    );
    Ok(())
}

/// A throwing transform keeps the pristine list and never empties the surface.
#[tokio::test]
async fn v2_broken_transform_keeps_the_pristine_prompt() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let st = with_transform_plugin(
        dir.path(),
        r#"export default {
  id: "boom",
  setup: async (ctx) => {
    await ctx.skill.transform(() => { throw new Error("nope"); });
  },
};"#,
    )
    .await?;

    reconcile(&st).await?;

    let after = prompt_skill_description(&st);
    assert!(
        after.contains("ORIGINAL-DESCRIPTION") && !after.is_empty(),
        "a throwing transform must leave the prompt intact, got: {after}"
    );
    let cmds = get_commands(&st).await;
    assert!(
        cmds.iter().any(|c| c["name"] == "demo-skill"),
        "the command surface must survive a broken plugin: {cmds:?}"
    );
    Ok(())
}
