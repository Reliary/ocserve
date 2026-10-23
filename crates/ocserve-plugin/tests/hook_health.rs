//! The 2026-10-23 review's highest-severity plugin findings, as tests.
//!
//! Each case asserts exactly the property that failed before the fix, so a
//! regression goes red rather than silently degrading (TESTING §1).

use serde_json::json;

fn node_present() -> bool {
    std::process::Command::new("node")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

async fn boot(plugin_js: &str, tag: &str) -> ocserve_plugin::Sidecar {
    let dir = std::env::temp_dir().join(format!(
        "ocserve-hook-health-{}-{tag}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let entry = dir.join("p.mjs");
    std::fs::write(&entry, plugin_js).unwrap();
    let host = ocserve_plugin::materialize_host(&dir.join("host")).unwrap();
    let mut sc = ocserve_plugin::Sidecar::spawn(&host, "http://127.0.0.1:9", "/w")
        .await
        .expect("spawn");
    sc.load(
        "t",
        &entry,
        &json!({"directory": "/w"}),
        &serde_json::Value::Null,
    )
    .await
    .expect("load");
    sc
}

/// A plugin tool returning a non-object `metadata` must not be able to kill
/// the process. Before the fix, Rust indexed it with `IndexMut<&str>`, which
/// panics, and release builds abort.
#[tokio::test]
async fn non_object_metadata_is_rejected_not_fatal() {
    if !node_present() {
        return;
    }
    let sc = boot(
        r#"export const hook = async () => ({
  tool: {
    bad_meta: {
      description: "returns array metadata",
      args: {},
      execute: async () => ({ title: "t", output: "ok", metadata: [] }),
    },
  },
});"#,
        "meta",
    )
    .await;
    let out = sc
        .handle()
        .execute_tool("bad_meta", json!({}), json!({"sessionID": "s"}))
        .await
        .expect("tool call");
    assert_eq!(out["ok"], true);
    assert!(
        out["metadata"].is_object(),
        "metadata must be coerced to an object at the boundary: {out}"
    );
}

/// A recycle must not make every hook a silent no-op. `registers()` has to
/// keep reporting the loaded plugins, and the hook path must recover (respawn
/// + replay) instead of returning before any counter moves.
#[tokio::test]
async fn recycle_does_not_silently_disable_plugins() {
    if !node_present() {
        return;
    }
    let mut sc = boot(
        r#"export const hook = async () => ({
  "tool.execute.after": async (_i, o) => { o.title = "survived"; },
});"#,
        "recycle",
    )
    .await;
    assert!(sc.handle().registers("tool.execute.after"));
    assert!(sc.recycle_child(), "recycle requested");
    assert!(
        sc.handle().registers("tool.execute.after"),
        "a recycle must not erase the loaded-plugin knowledge"
    );
    let out = sc
        .request_bounded(
            "trigger",
            json!({"name": "tool.execute.after", "input": {}, "output": {}}),
            ocserve_plugin::hook_budget("tool.execute.after"),
        )
        .await
        .expect("recovered hook");
    assert_eq!(
        out["title"], "survived",
        "replay must restore the hook: {out}"
    );
}

/// A dead generation must fail fast, not hang for the full budget.
#[tokio::test]
async fn dead_generation_fails_fast() {
    if !node_present() {
        return;
    }
    let mut sc = boot(
        r#"export const hook = async () => ({ "chat.message": async () => {} });"#,
        "dead",
    )
    .await;
    let handle = sc.handle();
    let pid = sc.child_pid();
    // Kill the child behind the owner's back: the reader marks the generation
    // dead, so the next request must not wait for the 30 s RPC budget.
    let _ = std::process::Command::new("kill")
        .args(["-9", &pid.to_string()])
        .status();
    // Give the reader a moment to observe EOF.
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    let t0 = std::time::Instant::now();
    let r = handle.request("v2_inventory", json!({})).await;
    assert!(r.is_err(), "a dead host must error");
    assert!(
        t0.elapsed() < std::time::Duration::from_secs(5),
        "must fail fast, took {:?}",
        t0.elapsed()
    );
    sc.shutdown().await;
}
