//! ocserve CLI: serve, import, doctor, bench.
#![cfg_attr(not(test), forbid(unsafe_code))]

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};

// musl's allocator is the default global allocator otherwise, and it retains
// freed pages (no malloc_trim equivalent). mimalloc exposes mi_collect, which
// the trim hook calls; glibc and Darwin keep the system allocator because their
// measured behaviour is already good and this is not a free change.
#[cfg(all(target_os = "linux", target_env = "musl"))]
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

#[derive(Parser)]
#[command(
    name = "ocserve",
    version,
    about = "Rust drop-in server for opencode v1"
)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Start the HTTP/SSE server (replaces `opencode serve`)
    Serve {
        /// Port. Precedence: --port flag > OCSERVE_PORT env > 4096 default
        /// (upstream's default; the hosted web UI at app.opencode.ai dials
        /// http://localhost:4096 for non-opencode.ai origins and
        /// `opencode attach` defaults to the same). Explicit flag wins over
        /// the env var; both override the default.
        #[arg(long)]
        port: Option<u16>,
        /// Hostname. Precedence: --hostname flag > OCSERVE_HOSTNAME env >
        /// 127.0.0.1 default (loopback-only unless overridden).
        #[arg(long)]
        hostname: Option<String>,
        /// Auth mode. Precedence: --auth-mode flag > OCSERVE_AUTH_MODE env >
        /// ocserve.toml [server].auth_mode > "off" default (v1 freeze parity:
        /// passwordless loopback). "basic" challenges EVERY route (incl. SSE,
        /// /metrics, fallback/405s) with upstream's exact 401
        /// (`UnauthorizedError`/`Authentication required` +
        /// `WWW-Authenticate: Basic realm="Secure Area"`).
        #[arg(long)]
        auth_mode: Option<String>,
        /// File containing the basic-auth password (mode 600 enforced at
        /// read; the path itself is never logged). Precedence:
        /// --auth-password-file flag > OCSERVE_AUTH_PASSWORD_FILE env >
        /// ocserve.toml [server].auth_password_file >
        /// OCSERVE_SERVER_PASSWORD env. Required when mode is "basic"
        /// (fail-fast otherwise). Deliberate deviation from upstream v2 UX:
        /// no generated-password-to-stdout (rule 8 absolute — secrets never
        /// in logs; upstream's `server password` line is read from THEIR log
        /// by pair-v2, never emitted by us).
        #[arg(long)]
        auth_password_file: Option<std::path::PathBuf>,
        #[arg(long)]
        data_dir: Option<std::path::PathBuf>,
    },
    /// Online backup: VACUUM INTO a fresh file (STORAGE §5 / cutover drill)
    Backup {
        /// Destination file (must not exist)
        #[arg(long)]
        dest: std::path::PathBuf,
        #[arg(long)]
        data_dir: Option<std::path::PathBuf>,
    },
    /// Boot self-checks: versions, pragmas, FTS, disk, config (SRE.md §1)
    Doctor {
        #[arg(long)]
        data_dir: Option<std::path::PathBuf>,
        /// Include the plugin inventory (dialect, hooks, bridged/unbridged).
        #[arg(long)]
        plugins: bool,
        /// Print the resolved resource budget for the running service
        /// (unit caps, per-class caps, current/peak, memory events, CPU).
        #[arg(long)]
        budget: bool,
    },
    /// Import last N sessions from a source opencode.db (read-only, PLAN §9)
    Import {
        #[arg(long)]
        source: std::path::PathBuf,
        #[arg(long, default_value_t = 20)]
        limit: u32,
        #[arg(long)]
        data_dir: Option<std::path::PathBuf>,
    },
    /// Differential replay of the recorded corpus against a target base URL
    /// (PLAN §6): --self-test runs against upstream (harness validity),
    /// otherwise point at ocserve to check wire compat.
    Replay {
        /// e.g. http://127.0.0.1:4901 (upstream) or http://127.0.0.1:4911 (ocserve)
        #[arg(long)]
        target: String,
        /// tolerate routes the target doesn't implement yet (ocserve during M1-M3)
        #[arg(long)]
        allow_missing: bool,
        /// second live target — gate becomes A↔B direct diff (pair mode;
        /// recorded-corpus freshness prints as info only)
        #[arg(long)]
        pair: Option<String>,
    },
    /// Model catalog maintenance (K-MODELS — upstream `opencode models refresh`)
    Models {
        #[command(subcommand)]
        action: ModelsCmd,
    },
}

#[derive(Subcommand)]
enum ModelsCmd {
    /// Fetch {OPENCODE_MODELS_URL||models.opencode.ai}/api.json NOW
    /// (freshness bypassed — upstream refresh(force))
    Refresh,
}

/// Port precedence: explicit `--port` > `OCSERVE_PORT` > ocserve.toml >
/// 4096 default. A malformed env value is a fail-fast boot error (SRE §1) —
/// a typo'd port silently serving elsewhere is worse than refusing to start.
fn resolve_port_with_file(flag: Option<u16>, file: Option<u16>) -> u16 {
    if let Some(p) = flag {
        return p;
    }
    match std::env::var("OCSERVE_PORT") {
        Ok(v) if !v.is_empty() => v
            .parse::<u16>()
            .unwrap_or_else(|_| panic!("OCSERVE_PORT is not a valid port number: {v:?}")),
        _ => file.unwrap_or(4096),
    }
}

/// Hostname precedence: explicit flag > env > default. Empty env = unset. Empty env = unset.
fn resolve_net(flag: Option<String>, env_key: &str, default: &str) -> String {
    if let Some(h) = flag {
        return h;
    }
    match std::env::var(env_key) {
        Ok(v) if !v.is_empty() => v,
        _ => default.to_string(),
    }
}

/// Data directory. Delegates to `platform` so the `HOME`-unset case is a named
/// error instead of silently landing in the process CWD (PLATFORM.md §1.2).
/// v1 `ConfigPluginV1.Spec` parsing: a string is options=Null, `[spec, options]`
/// is the tuple form, anything else is dropped (upstream schema rejects it).
fn plugin_specs(config: &serde_json::Value) -> Vec<(String, serde_json::Value)> {
    config
        .get("plugin")
        .and_then(|v| v.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|e| match e {
                    serde_json::Value::String(s) => Some((s.clone(), serde_json::Value::Null)),
                    serde_json::Value::Array(t) if !t.is_empty() => {
                        let spec = t.first().and_then(|v| v.as_str())?.to_string();
                        let opts = t.get(1).cloned().unwrap_or(serde_json::Value::Null);
                        Some((spec, opts))
                    }
                    _ => None,
                })
                .collect()
        })
        .unwrap_or_default()
}

fn default_data_dir() -> std::path::PathBuf {
    match platform::data_dir() {
        Ok(p) => p,
        Err(e) => {
            // Fatal, not a fallback: a database in the CWD is worse than no boot.
            eprintln!("ocserve: {e:#}");
            std::process::exit(2);
        }
    }
}

mod models_dev;
// The platform seam lives in ocserve-core so every crate shares one copy.
use ocserve_store::platform;
mod partition;
mod replay;
mod resilience;
mod runtime;
mod server_config;

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("ocserve=info")),
        )
        .with_writer(std::io::stderr)
        .init();
    let cli = Cli::parse();
    match cli.cmd {
        Cmd::Serve {
            port,
            hostname,
            auth_mode,
            auth_password_file,
            data_dir,
        } => {
            let data_dir = data_dir.unwrap_or_else(default_data_dir);
            // ocserve.toml is read AFTER the data dir resolves (it lives
            // inside it); missing file = defaults (bare-machine invariant).
            let file_cfg = server_config::load(&data_dir)?;
            rt::block_on(serve(
                resolve_net(
                    hostname,
                    "OCSERVE_HOSTNAME",
                    file_cfg.hostname.as_deref().unwrap_or("127.0.0.1"),
                ),
                resolve_port_with_file(port, file_cfg.port),
                auth_mode,
                auth_password_file,
                &file_cfg,
                data_dir,
            ))
        }
        Cmd::Doctor {
            data_dir,
            plugins,
            budget,
        } => rt::block_on(doctor(
            data_dir.unwrap_or_else(default_data_dir),
            plugins,
            budget,
        )),
        Cmd::Backup { dest, data_dir } => {
            let dir = data_dir.unwrap_or_else(default_data_dir);
            let db = ocserve_store::writer::db_path(&dir);
            // drill finding: VACUUM INTO on a READ_ONLY handle fails (SQLITE_READONLY);
            // open a writer conn (WAL + busy_timeout make this safe alongside a live server)
            let conn = ocserve_store::pragma::open_writer(&db)?;
            ocserve_store::backup_to(&conn, &dest)?;
            println!("backup: {} → {}", db.display(), dest.display());
            Ok(())
        }
        Cmd::Import {
            source,
            limit,
            data_dir,
        } => import(source, limit, data_dir.unwrap_or_else(default_data_dir)),
        Cmd::Replay {
            target,
            allow_missing,
            pair,
        } => rt::block_on(async move {
            if let Some(other) = pair {
                // Pair gate: the two LIVE targets compared to each other;
                // freshness vs recorded prints as info inside pair_all.
                let (compared, divergent, details) = replay::pair_all(&target, &other).await?;
                println!("pair {target} <-> {other}: {compared} compared, {divergent} divergent");
                for d in &details {
                    eprintln!("  {d}");
                }
                if divergent > 0 {
                    anyhow::bail!("pair diverged on {divergent} routes");
                }
                return Ok(());
            }
            let (pass, fail, failures) = replay::replay_all(&target, allow_missing).await?;
            println!("replay {target}: {pass} passed, {fail} failed");
            if fail > 0 {
                for f in &failures {
                    eprintln!("  {f}");
                }
                anyhow::bail!("differential replay failed: {fail} routes");
            }
            Ok(())
        }),
        Cmd::Models { action } => match action {
            ModelsCmd::Refresh => {
                let out = models_dev::refresh(true);
                println!("models refresh: {:?}", out);
                if out == models_dev::Outcome::Failed {
                    anyhow::bail!("models refresh failed (stale cache kept — see log)");
                }
                Ok(())
            }
        },
    }
}

mod rt {
    pub fn block_on<F: std::future::Future>(f: F) -> F::Output {
        // PERF-10X C5: floor of 8 workers, scales with the host (tokio's
        // own default would be avail_parallelism — we keep 8 as the floor
        // for small machines; .227 = 8 logical => 8 either way).
        let workers = std::cmp::max(
            8,
            std::thread::available_parallelism()
                .map(|n| n.get())
                .unwrap_or(8),
        );
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(workers)
            .thread_stack_size(1 << 20)
            // PERF-10X F1: store calls moved onto this pool — 16 threads
            // ≈ sub-ms queries × 8k+ ops/s with headroom for cold spikes;
            // budget bound: 16 threads × 4 MB parked reader cache = 64 MB.
            .max_blocking_threads(16)
            .enable_all()
            .build()
            .expect("tokio runtime")
            .block_on(f)
    }
}

async fn serve(
    hostname: String,
    port: u16,
    auth_mode: Option<String>,
    auth_password_file: Option<std::path::PathBuf>,
    file_cfg: &server_config::ServerCfg,
    data_dir: std::path::PathBuf,
) -> Result<()> {
    use ocserve_http::{AppState, FREEZE_VERSION};

    // SIGTERM/INT registration is the FIRST thing the server future does.
    // Registering at bind left the whole startup window (boot checks, partition,
    // config load, MCP probe, plugin load: ~5.5 s on this box) on the default
    // disposition, so `systemctl stop` during startup was SIGKILLed (observed
    // 2026-10-23 15:54). `Shutdown` holds the signal stream; it is consumed
    // only at `axum::serve`.
    let startup = install_shutdown();

    std::fs::create_dir_all(&data_dir)
        .with_context(|| format!("create data dir {}", data_dir.display()))?;
    // F4 (security review): the data dir holds the DB (provider API keys in
    // the `credential` table) and its `-wal`/`-shm` siblings, which SQLite
    // re-creates on checkpoint/WAL-reset with the process umask. A one-time
    // file chmod is defeated the moment they are recreated, so lock the
    // DIRECTORY to owner-only (0o700): a recreated WAL/SHM inherits nothing
    // readable by other users because they cannot traverse the dir at all.
    // The store's `restrict_file_mode` chmod stays as belt-and-braces for a
    // pre-existing 0644 DB. Unix only; documented no-op elsewhere.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&data_dir, std::fs::Permissions::from_mode(0o700))
            .with_context(|| format!("chmod 700 data dir {}", data_dir.display()))?;
    }

    // fail-fast boot checks (SRE.md §1) — before binding
    ocserve::boot_checks(&data_dir)?;

    // L2 partition (SRE §5): run BEFORE any child spawn (MCP probe / plugin
    // sidecar) so every wrapper can move itself into kids/. Scoped to
    // systemd invocations (INVOCATION_ID) — bare/test runs never restructure
    // a terminal's or harness's cgroup tree; OCSERVE_CGROUP_PARTITION=0/1 force.
    partition::setup();

    // Assemble config-derived payloads (fail fast if config unreadable)
    let rt = runtime::Runtime::load_for(&data_dir)
        .context("load runtime config (opencode.json/auth/models cache)")?;
    // M4a: probe MCP servers before serving (statuses captured, never fatal)
    let mcp_cfgs =
        ocserve_mcp::parse_config(rt.config.get("mcp").unwrap_or(&serde_json::Value::Null));
    // v1 config.plugin is `(string | [string, options])[]` (ConfigPluginV1.Spec).
    // The tuple form carries per-plugin options delivered to the server
    // factory; the plain-string form is options=Null. Non-string non-tuple
    // entries are dropped (upstream schema rejects them upstream of here).
    let plugin_specs = plugin_specs(&rt.config);
    let llm = rt.llm_registry()?;
    // W6: warn only about the EFFECTIVE default (what an unset prompt
    // actually resolves to — llm_registry's state→config precedence), not
    // every per-provider default-map entry: the first version flagged three
    // unused providers' defaults (user: "i don't use any of those 3 models")
    {
        let (dpid, dmid) = &llm.default_model;
        // catalog shape (live-verified): config_providers.providers is a
        // LIST of {id, models:{...}} — the earlier pointer("/providers/x/
        // models") never matched it; config-file providers are a DICT under
        // provider.<pid>.models. Both must be consulted.
        let known = rt
            .config_providers
            .get("providers")
            .and_then(|p| p.as_array())
            .and_then(|arr| {
                arr.iter()
                    .find(|e| e.get("id").and_then(|v| v.as_str()) == Some(dpid.as_str()))
            })
            .and_then(|e| e.get("models"))
            .and_then(|m| m.as_object())
            .map(|m| m.contains_key(dmid.as_str()))
            .unwrap_or(false)
            || rt
                .config
                .pointer(&format!("/provider/{dpid}/models"))
                .and_then(|m| m.as_object())
                .map(|m| m.contains_key(dmid.as_str()))
                .unwrap_or(false);
        if !dmid.is_empty() && !known {
            tracing::warn!(
                "effective default model {dpid}/{dmid} is NOT in the provider catalog — prompts with no model selection will fail at send time (the dead-ox-alpha case)"
            );
        }
    }
    // The plugin `config` hook (v1 parity) needs the resolved config; capture
    // it before `into_payloads` consumes rt.
    let mut plugin_config = rt.config.clone();
    // v1 derives `plugin_origins` before dispatching the `config` hook
    // (config.ts:113-115): each winning spec with the file and scope that
    // declared it. Freeze sends it, so a plugin that branches on scope sees it.
    if !plugin_specs.is_empty() {
        let origin = std::env::var("HOME")
            .map(|h| format!("{h}/.config/opencode"))
            .unwrap_or_default();
        let origins: Vec<serde_json::Value> = plugin_specs
            .iter()
            .map(|(spec, _)| serde_json::json!({"spec": spec, "source": origin, "scope": "global"}))
            .collect();
        plugin_config["plugin_origins"] = serde_json::Value::Array(origins);
    }
    let payloads = rt.into_payloads(); // consumed after llm/mcp/plugins took their pieces
    let db_path = ocserve_store::writer::db_path(&data_dir);
    let writer = std::sync::Arc::new(
        ocserve_store::Writer::spawn(db_path.clone()).context("spawn store writer")?,
    );
    let blobs = std::sync::Arc::new(
        ocserve_store::BlobStore::new(data_dir.join("blobs")).context("blob store")?,
    );
    let writer_for_sampler = writer.clone();
    let wal_path_sampler = db_path.with_extension("db-wal");
    let data_dir_sampler = data_dir.clone();
    // W1 one-time search backfill (idempotent/resumable; ~7.5s measured
    // scale for126k parts; later boots no-op via the parity check)
    match ocserve_store::backfill_part_search(&writer, &db_path) {
        Ok((0, 0, _)) => {}
        Ok((idx, skip, ms)) => {
            tracing::info!("search backfill: {idx} indexed, {skip} skipped, {ms}ms");
            ocserve_metrics::counter("ocserve_search_backfilled_total", idx);
        }
        Err(e) => {
            tracing::warn!("search backfill failed (search incomplete until next boot): {e:#}")
        }
    }
    // M6: compaction projection backfill (idempotent; legacy imports may
    // carry upstream compaction anchors — COMPACTION.md §8.1 resolved)
    match ocserve_store::backfill_compaction(&writer, &db_path) {
        Ok((0, 0, _)) => {}
        Ok((anchors, links, ms)) => {
            tracing::info!("compaction backfill: {anchors} anchors, {links} summary links, {ms}ms");
        }
        Err(e) => tracing::warn!("compaction backfill failed (retries next boot): {e:#}"),
    }
    // W6: search parity gauge (part rows vs projection) — mismatch after
    // backfill = indexed drift worth seeing on /metrics
    if let Ok(conn) = ocserve_store::pragma::open_reader(&db_path) {
        let parts: i64 = conn
            .query_row("SELECT count(*) FROM msg_part", [], |r| r.get(0))
            .unwrap_or(-1);
        let indexed: i64 = conn
            .query_row("SELECT count(*) FROM part_search", [], |r| r.get(0))
            .unwrap_or(-1);
        ocserve_metrics::gauge("ocserve_search_indexed_rows", indexed.max(0));
        if parts >= 0 && indexed >= 0 && indexed != parts {
            tracing::warn!(
                "search index parity drift: {indexed}/{parts} parts indexed (backfill next boot)"
            );
        }
    }
    // Password mode (SECURITY.md): off = freeze parity (passwordless
    // loopback); basic = upstream-exact 401 on every route. Resolved
    // flag > env > ocserve.toml > off; fail-fast on bad mode or missing
    // password (an open server by typo is the failure to avoid).
    let auth_off = server_config::resolve_auth_mode(auth_mode, file_cfg.auth_mode.as_deref())?;
    // F5 (security review): fail-fast when binding to a NON-loopback address
    // with auth OFF — the credential store echoes provider API keys on GET, so
    // an open non-loopback bind exposes them to the network.
    server_config::reject_open_nonloopback(&hostname, &auth_off)?;
    let auth = server_config::resolve_password(
        &auth_off,
        auth_password_file,
        file_cfg.auth_password_file.clone(),
    )?;
    let state = AppState::with_wiring(
        auth,
        payloads,
        ocserve_http::Wires {
            db: db_path,
            blobs,
            writer,
            llm,
        },
    );
    let hub = ocserve_mcp::McpHub::probe_all(&mcp_cfgs).await;
    // P2b: persist TOFU pins next to the DB so an offline rug-pull (server
    // edited while ocserve is down) trips drift at next boot
    if !mcp_cfgs.is_empty() {
        hub.set_pins_path(data_dir.join("mcp-pins.json"));
    }
    tracing::info!("mcp probe: {}", hub.statuses());
    ocserve_metrics::gauge(
        "ocserve_mcp_connected",
        hub.statuses()
            .as_object()
            .map(|o| o.values().filter(|v| v["status"] == "connected").count())
            .unwrap_or(0) as i64,
    );
    // P2 connect-time: enumerate + scan + TOFU-pin every server's tools NOW
    // (first-prompt laziness would defer the scan — the OWASP connect-time
    // channel wants it observed at connect; listing failures stay warnings)
    if !mcp_cfgs.is_empty() {
        let listed = hub.tool_schemas().await;
        tracing::info!("mcp trust: {} tool schemas scanned at boot", listed.len());
    }
    let _ = state.mcp.set(std::sync::Arc::new(hub));

    // W4: PATCH /config rebuilds derived payloads + LLM registry through
    // this closure (Runtime::load reads opencode.json + overlay; registry
    // layers legacy auth + auth-overlay — H2 hot-swaps endpoints/keys too)
    {
        let data_dir_for_reload = data_dir.clone();
        let reloader: std::sync::Arc<
            dyn Fn() -> anyhow::Result<(ocserve_http::Payloads, ocserve_http::LlmRegistry)>
                + Send
                + Sync,
        > = std::sync::Arc::new(move || {
            let rt = crate::runtime::Runtime::load_for(&data_dir_for_reload)?;
            let registry = rt.llm_registry()?; // borrow first — into_payloads moves
            Ok((rt.into_payloads(), registry)) // move, not clone (measured OOM)
        });
        *state.reloader.write() = Some(reloader);
    }
    // Plugin `config` hook result → the SAME swap path as a reload, but from
    // an explicit value (a hook mutation must not be silently discarded, and
    // must not race the files the reloader re-reads).
    {
        let data_dir_for_derive = data_dir.clone();
        let deriver: std::sync::Arc<
            dyn Fn(
                    serde_json::Value,
                )
                    -> anyhow::Result<(ocserve_http::Payloads, ocserve_http::LlmRegistry)>
                + Send
                + Sync,
        > = std::sync::Arc::new(move |cfg| {
            let rt = crate::runtime::Runtime::from_config(&data_dir_for_derive, cfg)?;
            let registry = rt.llm_registry()?;
            Ok((rt.into_payloads(), registry))
        });
        *state.deriver.write() = Some(deriver);
    }

    // H1: external-edit hot-reload — poll the exact files Runtime reads,
    // reconcile on change (fail-safe: broken config keeps old state serving)
    state.watch.write().paths = crate::runtime::watch_paths(&data_dir);
    ocserve_http::watch::observe(&state);
    // boot: hand the initial load pipeline's freed pages back (Tier-0 lab:
    // full catalog settles 237→91 MB with trim; tiny-catalog control = 19 MB)
    ocserve_http::watch::trim_heap();
    // periodic safety net: slow churn (sessions, searches) between reloads
    // still accumulates free-list pages; OCSERVE_TRIM_SECS=0 disables.
    if std::env::var("OCSERVE_TRIM").is_err() || std::env::var("OCSERVE_TRIM").as_deref() != Ok("0")
    {
        let trim_secs: u64 = std::env::var("OCSERVE_TRIM_SECS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(300);
        if trim_secs > 0 {
            tokio::spawn(async move {
                loop {
                    tokio::time::sleep(std::time::Duration::from_secs(trim_secs)).await;
                    ocserve_http::watch::trim_heap();
                }
            });
        }
    }
    tokio::spawn(ocserve_http::watch::watch_loop(
        state.clone(),
        ocserve_http::watch::WatchOpts::from_env(),
    ));

    // K-MODELS: same-as-upstream catalog refresh — immediate-if-stale then
    // every 60 minutes (models-dev.ts Schedule.spaced). Blocking fetch runs
    // off the async workers; fail-soft loop (stale cache keeps serving).
    tokio::spawn(async move {
        loop {
            let res = tokio::task::spawn_blocking(|| models_dev::refresh(false)).await;
            match res {
                Ok(outcome) => tracing::debug!("models catalog refresh: {outcome:?}"),
                Err(e) => tracing::warn!("models refresh task join error: {e}"),
            }
            tokio::time::sleep(std::time::Duration::from_secs(
                models_dev::REFRESH_INTERVAL_SECS,
            ))
            .await;
        }
    });

    // Legacy→ocserve delta sync (development bridge; kill switch
    // OCSERVE_LEGACY_SYNC=0). 60s cadence after a 5s settle, fail-soft: any
    // source error logs + counts, the task never dies.
    if ocserve_http::sync::sync_enabled() {
        let st = state.clone();
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_secs(5)).await;
            let mut tick = tokio::time::interval(std::time::Duration::from_secs(60));
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                tick.tick().await;
                // K-EFFICIENCY: tick cost visibility (the idle-jump suspect
                // until measured otherwise — 11 msgs total, so likely tiny).
                // Perf dive 2026-10-11: sync_tick is a synchronous fn doing
                // many reader opens + prepared statements + blob fsyncs; it
                // previously ran INLINE in this async task, parking a tokio
                // worker every 60 s. Run it on the blocking pool.
                let st_worker = st.clone();
                let sync_t0 = std::time::Instant::now();
                let tick_result = match tokio::task::spawn_blocking(move || {
                    ocserve_http::sync::sync_tick(&st_worker)
                })
                .await
                {
                    Ok(r) => r,
                    Err(e) => Err(anyhow::anyhow!("sync tick join: {e}")),
                };
                ocserve_metrics::observe(
                    "ocserve_sync_tick",
                    if tick_result.is_ok() {
                        "result=\"ok\""
                    } else {
                        "result=\"error\""
                    },
                    sync_t0.elapsed().as_micros() as u64,
                );
                match tick_result {
                    Ok(s) => {
                        if s.messages > 0 || s.backlog {
                            tracing::info!(
                                "legacy sync: {} messages, {} parts, {} sessions, backlog={}",
                                s.messages,
                                s.parts,
                                s.sessions,
                                s.backlog
                            );
                        }
                        ocserve_metrics::counter("ocserve_sync_messages_total", s.messages);
                        ocserve_metrics::gauge(
                            "ocserve_sync_last_epoch_seconds",
                            std::time::SystemTime::now()
                                .duration_since(std::time::UNIX_EPOCH)
                                .map(|d| d.as_secs() as i64)
                                .unwrap_or(0),
                        );
                    }
                    Err(e) => {
                        ocserve_metrics::counter("ocserve_sync_errors_total", 1);
                        tracing::warn!("legacy sync tick failed: {e:#}");
                    }
                }
            }
        });
    }

    // SRE §2 sampler (15s): rss/peak, wal, queue, sidecar + lifecycle gauges
    // (antagonism W3: transient maps MUST be watchable — leak-audit test
    // covers CI, this covers the live soak) + bounded storage maintenance.
    {
        let writer_m = writer_for_sampler.clone();
        let wal_path = wal_path_sampler.clone();
        let st = state.clone();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(std::time::Duration::from_secs(15));
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            let mut n: u64 = 0;
            let mut recycle_hi: u32 = 0;
            let mut warned_locks: std::collections::HashSet<String> =
                std::collections::HashSet::new();
            // 10 minutes: the tool's own cap is 120 s (600 s max), so anything
            // past this is not a slow command.
            const LOCK_WATCHDOG: std::time::Duration = std::time::Duration::from_secs(600);
            loop {
                tick.tick().await;
                n += 1;
                ocserve_metrics::sample_rss();
                ocserve_metrics::gauge("ocserve_writer_queue_depth", writer_m.queue_depth());
                let wal = std::fs::metadata(&wal_path)
                    .map(|m| m.len() as i64)
                    .unwrap_or(0);
                ocserve_metrics::gauge("ocserve_wal_bytes", wal);
                // MEMORY §7.6: DB growth KPI (thousands-of-sessions target —
                // gauged now, policy at ≥5 GB / ≥1k sessions).
                if let Some(db) = wal_path
                    .to_str()
                    .and_then(|w| w.strip_suffix("-wal"))
                    .and_then(|d| std::fs::metadata(d).ok())
                {
                    ocserve_metrics::gauge("ocserve_db_bytes", db.len() as i64);
                }
                // Precise sidecar RSS: "first node/bun child of mine" is the
                // plugin browser host after any sidecar respawn (pre-existing
                // attribution flake) — ask the sidecar for ITS child pid.
                let mut sidecar_rss_exact: Option<i64> = None;
                if let Some(plug) = st.plugins.get() {
                    let sc = plug.lock().await;
                    sidecar_rss_exact = resilience::rss_of_pid(sc.child_pid());
                }
                let rss = sidecar_rss_exact.or_else(|| sidecar_rss(std::process::id()));
                if let Some(r) = rss {
                    ocserve_metrics::gauge("ocserve_sidecar_rss_bytes", r);
                    // L1 graceful recycle (SRE §5): the gauge we already take,
                    // turned into action before the kernel ever sees the cap.
                    let thr = resilience::recycle_threshold_mb();
                    let r_mb = (r / 1_048_576) as u64;
                    if thr > 0 && r_mb >= thr {
                        recycle_hi = recycle_hi.saturating_add(1);
                    } else {
                        recycle_hi = 0;
                    }
                    if recycle_hi >= 2
                        && let Some(plug) = st.plugins.get()
                        // busy lock ⇒ RPC in flight ⇒ not idle ⇒ skip tick
                        && let Ok(mut sc) = plug.try_lock()
                        && resilience::should_recycle(
                            thr,
                            r_mb,
                            recycle_hi,
                            sc.in_flight(),
                            sc.uptime().as_secs(),
                        )
                        && sc.recycle_child()
                    {
                        recycle_hi = 0;
                        ocserve_metrics::labeled_counter(
                            "ocserve_sidecar_recycle_total",
                            "reason=\"rss\"",
                            1,
                        );
                        tracing::info!(
                            "plugin sidecar recycled (rss {r_mb}MB >= {thr}MB, idle) — respawn on next trigger"
                        );
                    }
                }
                // PSI instrumentation (measure-first gate for any D-future
                // MemoryHigh soft throttle; absent on cgroup v1 → no gauge)
                if let Some(v) = resilience::psi_avg10_centi() {
                    ocserve_metrics::gauge("ocserve_mem_pressure_avg10", v);
                }

                // L2 kids accounting (only when the dance published one)
                if let Some(kp) = partition::kids_path()
                    && let Some(v) = std::fs::read_to_string(kp.join("memory.current"))
                        .ok()
                        .and_then(|s| s.trim().parse::<i64>().ok())
                {
                    ocserve_metrics::gauge("ocserve_kids_bytes", v);
                }
                // Lock age (visibility, never cancellation): a wedged tool
                // call cannot be cancelled in the blocking pool, so the only
                // honest signal is "this session has been held for N seconds".
                // 2026-10-21: a backgrounded AND-list held the tool's pipe, the
                // lock never released, and nothing in metrics said so.
                ocserve_metrics::gauge("ocserve_prompt_locks", st.prompt_locks.lock().len() as i64);
                let (oldest, stale) = {
                    let aged = st.lock_ages.lock();
                    let oldest = aged
                        .values()
                        .map(|t| t.elapsed().as_secs())
                        .max()
                        .unwrap_or(0);
                    let stale: Vec<(String, u64)> = aged
                        .iter()
                        .filter(|(_, t)| t.elapsed() >= LOCK_WATCHDOG)
                        .map(|(sid, t)| (sid.clone(), t.elapsed().as_secs()))
                        .collect();
                    (oldest, stale)
                };
                ocserve_metrics::gauge("ocserve_prompt_lock_age_seconds", oldest as i64);
                for (sid, secs) in stale {
                    if warned_locks.insert(sid.clone()) {
                        tracing::warn!(
                            "session {sid} has held the prompt lock for {secs}s — a tool call \
                             may be wedged (ocserve_prompt_lock_age_seconds); restart the unit \
                             to reap it (SRE §5.7)"
                        );
                    }
                }
                {
                    let live = st.lock_ages.lock();
                    warned_locks.retain(|s| live.contains_key(s));
                }
                ocserve_metrics::gauge("ocserve_prompt_tasks", st.prompt_tasks.lock().len() as i64);
                ocserve_metrics::gauge(
                    "ocserve_question_pending",
                    st.question_gate.list().len() as i64,
                );
                ocserve_metrics::gauge("ocserve_permission_pending", st.gate.pending_len() as i64);
                if let Ok(c) = ocserve_store::session_count(&st.db) {
                    ocserve_metrics::gauge("ocserve_sessions_total", c);
                }
                // Storage maintenance (STORAGE §2 amendment: operational
                // pragmas run on the WRITER connection — config pragmas stay
                // in open()): drain freelist a bounded 4096 pages/tick (no-op
                // at freelist=0), refresh planner stats hourly. Never a
                // full VACUUM (online cost) — incremental only.
                let w = writer_m.clone();
                let optimize = n.is_multiple_of(240);
                let join = tokio::task::spawn_blocking(move || {
                    let mut ops = vec![ocserve_store::WriteOp::Sql {
                        sql: "PRAGMA incremental_vacuum(4096)".into(),
                        params: vec![],
                    }];
                    if optimize {
                        ops.push(ocserve_store::WriteOp::Sql {
                            sql: "PRAGMA optimize".into(),
                            params: vec![],
                        });
                    }
                    w.write(ops)
                })
                .await;
                match join {
                    Err(e) => tracing::error!("maintenance task join: {e}"),
                    Ok(Err(e)) => tracing::error!("storage maintenance: {e:#}"),
                    Ok(Ok(_)) => {}
                }
                // SNAPSHOT.md §4: prune shadow-git repos hourly
                // (`git gc --prune=7.days`), mirroring upstream's interval.
                if optimize {
                    let dd = data_dir_sampler.clone();
                    let pruned = tokio::task::spawn_blocking(move || {
                        ocserve_core::snapshot::Snapshots::gc_prune_all(&dd)
                    })
                    .await
                    .unwrap_or(0);
                    if pruned > 0 {
                        tracing::info!("snapshot: pruned {pruned} shadow repo(s)");
                    }
                }
            }
        });
    }

    // M4b: plugin sidecar — materialize host, spawn Node, load configured
    // plugins (statuses logged like /mcp; load failures never block serving)
    let mut plugin_sidecar: Option<ocserve_plugin::Sidecar> = None;
    if !plugin_specs.is_empty() {
        let host = ocserve_plugin::materialize_host(&data_dir.join("plugin-host"))
            .context("materialize plugin host")?;
        let home = std::path::PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| "/".into()));
        let server_url = format!("http://{hostname}:{port}");
        let directory = std::env::current_dir()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|_| "/".into());
        match ocserve_plugin::Sidecar::spawn(&host, &server_url, &directory).await {
            Ok(mut sc) => {
                // D1: normalization root (data-dir emit for self-contained
                // bundles; kill switch OCSERVE_PLUGIN_NORMALIZE=0)
                sc.set_normalize_root(data_dir.join("normalized"));
                let input = serde_json::json!({
                    "directory": directory,
                    "projectID": "global",
                    "worktree": "/",
                    "serverUrl": server_url,
                });
                for (spec, options) in &plugin_specs {
                    match ocserve_plugin::resolve_entry(spec, &home) {
                        Ok(entry) => match sc.load(spec, &entry, &input, options).await {
                            Ok(hooks) => {
                                ocserve_metrics::labeled_counter(
                                    "ocserve_plugin_load_total",
                                    "result=\"ok\"",
                                    1,
                                );
                                tracing::info!("plugin {spec} loaded: {hooks:?}")
                            }
                            Err(e) => {
                                // A load failure used to be one boot WARN, after
                                // which the plugin was skipped for the life of
                                // the process with no signal (review H2). Count
                                // it and name the spec.
                                ocserve_metrics::labeled_counter(
                                    "ocserve_plugin_load_total",
                                    "result=\"error\"",
                                    1,
                                );
                                tracing::error!("plugin {spec} load failed: {e:#}")
                            }
                        },
                        Err(e) => {
                            ocserve_metrics::labeled_counter(
                                "ocserve_plugin_load_total",
                                "result=\"unresolved\"",
                                1,
                            );
                            tracing::error!("plugin {spec} resolve failed: {e:#}")
                        }
                    }
                }
                // v1 parity (plugin/index.ts:244-251): notify every loaded
                // plugin of the resolved config via its `config` hook (called
                // once, errors logged + ignored). Previously never dispatched —
                // a registered-but-never-called hook (TESTING §1.6 class).
                match sc.config(plugin_config.clone()).await {
                    Ok(v) => {
                        tracing::info!(
                            "plugin config hook: matched={}",
                            v.get("matched").and_then(|m| m.as_u64()).unwrap_or(0)
                        );
                        if let Some(cfg) = v.get("config").filter(|c| c.is_object()) {
                            plugin_config = cfg.clone();
                        }
                    }
                    Err(e) => tracing::warn!("plugin config dispatch failed: {e:#}"),
                }
                tracing::info!("plugin statuses: {}", sc.statuses());
                plugin_sidecar = Some(sc);
            }
            Err(e) => tracing::error!("plugin sidecar spawn failed: {e:#}"),
        }
    }
    if let Some(sc) = plugin_sidecar {
        let _ = state
            .plugins
            .set(std::sync::Arc::new(tokio::sync::Mutex::new(sc)));
        // P0e: bus → plugin `event` hook pump (v1 plugin/index.ts:255-259)
        ocserve_http::start_plugin_event_pump(&state);
        // Tier 2 (2026-10-18): apply v2 command/skill transforms at BOOT too,
        // not only on the next config reload — otherwise a plugin's commands
        // and skills are missing until the operator edits a config file, and
        // the skills block (which the MODEL reads every turn) would silently
        // omit them. Same window discipline as reconcile: transform, then
        // re-derive the wire cache, so `GET /command`, the skills block and
        // the precomputed bytes agree.
        let st = state.clone();
        let cfg_after_hook = plugin_config.clone();
        tokio::spawn(async move {
            // Apply the plugin's config mutation FIRST (if any), then the v2
            // command/skill pull. Both go through the same window: a plugin
            // that adds a command via `config` and one that transforms the
            // list must not race.
            // `apply_config_value` already derives payloads AND pulls the v2
            // command/skill transforms in one swap window. A following
            // `reconcile()` would re-read the FILE and overwrite the plugin's
            // mutation milliseconds later (review H-3), silently reverting a
            // plugin that TIGHTENS permissions. Apply once; on failure keep
            // the file-derived config and say so.
            match ocserve_http::watch::apply_config_value(&st, cfg_after_hook).await {
                Ok(()) => {}
                Err(e) => {
                    tracing::warn!("plugin config apply failed: {e:#} (previous config kept)");
                    ocserve_metrics::labeled_counter(
                        "ocserve_plugin_config_apply_total",
                        "result=\"error\"",
                        1,
                    );
                    if let Err(e) = ocserve_http::watch::reconcile(&st).await {
                        tracing::error!("config reload after a failed plugin apply: {e:#}");
                    }
                }
            }
        });
    }
    let app = ocserve_http::router(state.clone());

    // W3 opt-in prewarm (default off): fetch the entry HTML's referenced
    // /assets/* through the proxy once so the first browser load hits the
    // lazy cache. Bounded (a handful of assets); failures are logged, never
    // fatal — the UI must boot with or without the upstream reachable.
    if state.ui.enabled && state.ui.prewarm {
        let st = state.clone();
        tokio::spawn(async move {
            if let Err(e) = ocserve_http::ui::prewarm_assets(&st).await {
                tracing::warn!("ui prewarm: {e:#}");
            }
        });
    }

    let addr = format!("{hostname}:{port}");
    let listener = tokio::net::TcpListener::bind(&addr)
        .await
        .with_context(|| format!("bind {addr} (port in use?)"))?;
    // TEST SEAM (debug builds only): hold the bind→serve window open so the
    // at-accept SIGTERM test can hit it deterministically. Compiled out of
    // release binaries; it never delays a real service.
    #[cfg(debug_assertions)]
    if let Some(ms) = std::env::var("OCSERVE_TEST_BIND_DELAY_MS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
    {
        tokio::time::sleep(std::time::Duration::from_millis(ms)).await;
    }
    tracing::info!("ocserve serving on http://{addr} (freeze {FREEZE_VERSION})");
    // Onboarding: name both client entry points the URL is good for, so a
    // first-run user knows what to do with it (the port question came up
    // twice in practice).
    tracing::info!("  TUI:     opencode attach http://{addr}");
    tracing::info!("  web UI:  open http://{addr}");

    // Perimeter CORS (Phase 1 auth work): preflights must 204 before
    // routing/auth/fallbacks see them (fallback + MNF responses bypass router
    // middleware when layers are registered before .fallback() — proven
    // empirically). Router::layer wraps the fully-built router including its
    // fallbacks; only short-circuits preflights, passes everything else.
    let app = app.layer(axum::middleware::from_fn(ocserve_http::ui::perimeter_cors));
    // The drain deadline starts AFTER the signal. Wrapping the signal wait in
    // a timeout (the first version of this fix) shut the server down 20 s after
    // every start, because the timer was already running while waiting.
    let (signalled_tx, signalled_rx) = tokio::sync::oneshot::channel::<()>();
    let serve = axum::serve(listener, app).with_graceful_shutdown(async move {
        startup.wait().await;
        let _ = signalled_tx.send(());
    });
    // `WithGracefulShutdown` implements `IntoFuture`, not `Future`.
    let serve = serve.into_future();
    tokio::pin!(serve);
    let drain_secs: u64 = std::env::var("OCSERVE_DRAIN_SECS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(20);
    if drain_secs == 0 {
        serve.await?;
    } else {
        tokio::select! {
            r = &mut serve => r?,
            _ = async {
                // Only starts counting once the shutdown future has fired.
                let _ = signalled_rx.await;
                tokio::time::sleep(std::time::Duration::from_secs(drain_secs)).await;
            } => {
                tracing::warn!(
                    "shutdown: drain exceeded {drain_secs}s - exiting with connections open"
                );
            }
        }
    }
    if let Some(plug) = state.plugins.get() {
        plug.lock().await.shutdown().await;
        tracing::info!("plugin sidecar disposed");
    }
    Ok(())
}

/// RSS of the plugin sidecar child (bun or node — whichever runtime is
/// active; comm is "bun" or "node-MainThread").
fn sidecar_rss(ppid: u32) -> Option<i64> {
    let mut found = None;
    // NOTE: `?` must not appear here — /proc contains non-pid entries
    // (cpuinfo, meminfo, …) and a single parse miss must not abort the scan.
    for entry in std::fs::read_dir("/proc").ok()? {
        let name = match entry {
            Ok(e) => e.file_name(),
            Err(_) => continue,
        };
        let Ok(pid) = name.to_string_lossy().parse::<u32>() else {
            continue;
        };
        let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat")) else {
            continue;
        };
        // comm can contain spaces/parens: split after the last ')'
        let Some((_, rest)) = stat.rsplit_once(')') else {
            continue;
        };
        let fields: Vec<&str> = rest.split_whitespace().collect();
        if fields.len() > 1 && fields[1].parse::<u32>().ok() == Some(ppid) {
            let comm = std::fs::read_to_string(format!("/proc/{pid}/comm")).ok()?;
            let c = comm.trim();
            if c.starts_with("node") || c == "bun" || c.starts_with("bun-") {
                // node: comm shows "node-MainThread"; bun: "bun"
                let status = std::fs::read_to_string(format!("/proc/{pid}/status")).ok()?;
                found = status
                    .lines()
                    .find(|l| l.starts_with("VmRSS:"))
                    .and_then(|l| l.split_whitespace().nth(1))
                    .and_then(|v| v.parse::<i64>().ok())
                    .map(|kb| kb * 1024);
                break;
            }
        }
    }
    found
}

/// Shutdown signal, installed BEFORE the listener binds.
///
/// `shutdown_signal()` used to create the signal stream only when the returned
/// future was first polled, which happens inside `axum::serve(...)`. That left
/// a window between bind and poll where SIGTERM took the default disposition
/// and killed the process without draining (observed as `ExitStatus(15)` in a
/// full parallel suite run, 2026-10-23). Registration now happens first; the
/// future only waits.
#[cfg(unix)]
struct Shutdown {
    term: Option<tokio::signal::unix::Signal>,
}

#[cfg(unix)]
fn install_shutdown() -> Shutdown {
    use tokio::signal::unix::{SignalKind, signal};
    let term = match signal(SignalKind::terminate()) {
        Ok(s) => Some(s),
        Err(e) => {
            tracing::warn!("SIGTERM handler unavailable: {e}; waiting on ctrl_c only");
            None
        }
    };
    Shutdown { term }
}

#[cfg(unix)]
impl Shutdown {
    async fn wait(mut self) {
        match self.term.as_mut() {
            Some(term) => {
                tokio::select! {
                    _ = tokio::signal::ctrl_c() => {}
                    _ = term.recv() => {}
                }
            }
            None => {
                let _ = tokio::signal::ctrl_c().await;
            }
        }
        tracing::info!("shutdown: draining");
    }
}

fn read_first(path: &str) -> Option<String> {
    std::fs::read_to_string(path)
        .ok()
        .map(|s| s.trim().to_string())
}

/// `doctor --budget`: what the SERVICE actually has, read from its live
/// cgroup. The doctor process is not the service, so it asks systemd for the
/// unit's cgroup path instead of using its own.
fn budget_report() {
    let mut sc = std::process::Command::new("systemctl");
    sc.args(["--user", "show", "ocserve.service", "-p", "ControlGroup"]);
    let cg = sc
        .output()
        .ok()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_default();
    let rel = cg.strip_prefix("ControlGroup=").unwrap_or("").trim();
    if rel.is_empty() {
        let unit = std::path::PathBuf::from(std::env::var("HOME").unwrap_or_default())
            .join(".config/systemd/user/ocserve.service");
        println!(
            "  budget:          {}",
            if unit.exists() {
                "service installed but not running (no cgroup)"
            } else {
                "service not installed"
            }
        );
        return;
    }
    let root = std::path::PathBuf::from("/sys/fs/cgroup").join(rel.trim_start_matches('/'));
    let mb = |v: Option<String>| {
        v.and_then(|s| s.parse::<u64>().ok())
            .filter(|b| *b != u64::MAX)
            .map(|b| format!("{}M", b / 1024 / 1024))
            .unwrap_or_else(|| "unlimited".into())
    };
    let node = |p: &std::path::Path, file: &str| mb(read_first(&p.join(file).to_string_lossy()));
    let stat = |p: &std::path::Path, key: &str| {
        read_first(&p.join("memory.stat").to_string_lossy()).and_then(|s| {
            s.lines()
                .find_map(|l| l.strip_prefix(&format!("{key} ")))
                .and_then(|v| v.parse::<u64>().ok())
                .map(|b| format!("{}M", b / 1024 / 1024))
        })
    };
    let events = |p: &std::path::Path| {
        read_first(&p.join("memory.events").to_string_lossy())
            .map(|s| s.replace('\n', " "))
            .unwrap_or_default()
    };
    println!("  budget:          {}", rel);
    println!(
        "    parent        high={} max={} anon={}",
        node(&root, "memory.high"),
        node(&root, "memory.max"),
        stat(&root, "anon").unwrap_or_else(|| "?".into())
    );
    for (name, label) in [
        ("main", "server"),
        ("kids/plugin", "plugin"),
        ("kids/mcp", "mcp"),
        ("kids/tools", "tools"),
        ("kids/pty", "pty"),
    ] {
        let p = root.join(name);
        if !p.is_dir() {
            continue;
        }
        println!(
            "    {label:<13} high={:<10} max={:<10} cur={:<10} peak={:<10} cpu.weight={:<5} events: {}",
            node(&p, "memory.high"),
            node(&p, "memory.max"),
            node(&p, "memory.current"),
            node(&p, "memory.peak"),
            read_first(&p.join("cpu.weight").to_string_lossy()).unwrap_or_else(|| "-".into()),
            events(&p)
        );
    }
    // Per-node pressure, not just the aggregate: a class hitting its high shows
    // here even when the parent is calm.
    for (name, label) in [
        ("main", "server"),
        ("kids/plugin", "plugin"),
        ("kids/tools", "tools"),
    ] {
        let p = root.join(name);
        if let Some(pr) = read_first(&p.join("memory.pressure").to_string_lossy()) {
            println!("    {label:<13} pressure {}", pr.replace('\n', " | "));
        }
    }
    let managed = read_first(&root.join("memory.max").to_string_lossy()) != Some("max".into());
    println!(
        "    mode          {}{}",
        if managed {
            "managed"
        } else {
            "unmanaged/host-governed"
        },
        if crate::partition::kids_path().is_some() {
            ""
        } else {
            " (partition not active in this process)"
        }
    );
    if let Some(cpu) = read_first(&root.join("cpu.stat").to_string_lossy()) {
        let keep: Vec<&str> = cpu
            .lines()
            .filter(|l| {
                l.starts_with("nr_periods")
                    || l.starts_with("nr_throttled")
                    || l.starts_with("throttled_usec")
            })
            .collect();
        if !keep.is_empty() {
            println!("    cpu.stat      {}", keep.join(" "));
        }
    }
}

async fn doctor(data_dir: std::path::PathBuf, plugins: bool, budget: bool) -> Result<()> {
    println!("ocserve doctor");
    println!("  sqlite version:  {}", rusqlite::version());
    match ocserve_store::pragma::assert_version_ok() {
        Ok(()) => println!(
            "  version gate:    OK (>= {})",
            ocserve_store::pragma::MIN_SQLITE_VERSION
        ),
        Err(e) => println!("  version gate:    FAIL: {e:#}"),
    }
    // The service is an OPTIONAL overlay — informational, never a failure
    // (foreground `ocserve serve` is the contract; preferred: scripts/install.sh).
    let unit = std::path::PathBuf::from(std::env::var("HOME").unwrap_or_default())
        .join(".config/systemd/user/ocserve.service");
    println!(
        "  service:        {}",
        if unit.exists() {
            "installed (optional overlay; scripts/uninstall.sh removes)"
        } else {
            "not installed (optional overlay; preferred: scripts/install.sh)"
        }
    );
    // Tailnet exposure overlay (2026-10-12: a reboot left the socat forwarder
    // dead-and-DISABLED while loopback stayed healthy — tailnet clients got
    // connection refused and nothing anywhere said so). Informational only:
    // the forwarder is per-host (bind IP), installer never manages it.
    let fwd_unit = std::path::PathBuf::from(std::env::var("HOME").unwrap_or_default())
        .join(".config/systemd/user/ocserve-tailscale-forward.service");
    if fwd_unit.exists() {
        let state = std::process::Command::new("systemctl")
            .args(["--user", "is-active", "ocserve-tailscale-forward"])
            .output()
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
            .unwrap_or_default();
        let enabled = std::process::Command::new("systemctl")
            .args(["--user", "is-enabled", "ocserve-tailscale-forward"])
            .output()
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
            .unwrap_or_default();
        println!("  tailnet fwd:    {state} ({enabled}; 100.x:4912 -> 127.0.0.1:4912)");
    }
    let db = ocserve_store::writer::db_path(&data_dir);
    if db.exists() {
        match ocserve_store::pragma::open_writer(&db) {
            Ok(conn) => {
                let ver: i64 = conn
                    .query_row("PRAGMA user_version", [], |r| r.get(0))
                    .unwrap_or(-1);
                let free: i64 = conn
                    .query_row("PRAGMA freelist_count", [], |r| r.get(0))
                    .unwrap_or(-1);
                let pages: i64 = conn
                    .query_row("PRAGMA page_count", [], |r| r.get(0))
                    .unwrap_or(-1);
                let jm: String = conn
                    .query_row("PRAGMA journal_mode", [], |r| r.get(0))
                    .unwrap_or_default();
                let av: i64 = conn
                    .query_row("PRAGMA auto_vacuum", [], |r| r.get(0))
                    .unwrap_or(0);
                println!("  schema version:  {ver}");
                println!("  journal_mode:    {jm}");
                println!("  auto_vacuum:     {av} (1=INCREMENTAL)");
                println!("  pages:           {pages} (freelist {free})");
                match conn.query_row("PRAGMA quick_check", [], |r| r.get::<_, String>(0)) {
                    Ok(r) if r == "ok" => println!("  quick_check:     ok"),
                    Ok(r) => println!("  quick_check:     {r}"),
                    Err(e) => println!("  quick_check:     FAIL: {e}"),
                }
                // W6: search index parity + trigram smoke
                let sp: i64 = conn
                    .query_row("SELECT count(*) FROM msg_part", [], |r| r.get(0))
                    .unwrap_or(-1);
                let si: i64 = conn
                    .query_row("SELECT count(*) FROM part_search", [], |r| r.get(0))
                    .unwrap_or(-1);
                if sp == si {
                    println!("  search parity:   ok ({si} rows)");
                } else {
                    println!("  search parity:   DRIFT {si}/{sp} (boot backfill repairs)");
                }
                match conn.query_row(
                    "SELECT count(*) FROM part_search_fts WHERE part_search_fts MATCH '\"idx\"'",
                    [],
                    |r| r.get::<_, i64>(0),
                ) {
                    Ok(n) => println!("  trigram MATCH:   ok ({n} rows contain 'idx')"),
                    Err(e) => println!("  trigram MATCH:   FAIL: {e}"),
                }
            }
            Err(e) => println!("  open:            FAIL: {e:#}"),
        }
    } else {
        println!("  database:        not created yet ({})", db.display());
    }
    if budget {
        budget_report();
    }
    if plugins {
        plugin_report(&data_dir).await;
    }
    Ok(())
}

/// `doctor --plugins`: what the configured plugins register, what ocserve
/// bridges, and the last internal failure. This is the support instrument for
/// an install we cannot see: a configured-but-silently-broken plugin was the
/// 2026-10-22 incident, and nothing in the server said so.
async fn plugin_report(data_dir: &std::path::Path) {
    let home = std::path::PathBuf::from(std::env::var("HOME").unwrap_or_default());
    let rt = match crate::runtime::Runtime::load_for(data_dir) {
        Ok(rt) => rt,
        Err(e) => {
            println!("  plugins:         FAIL to read config: {e:#}");
            return;
        }
    };
    let specs = crate::plugin_specs(&rt.config);
    if specs.is_empty() {
        println!("  plugins:         none configured");
        return;
    }
    let host = match ocserve_plugin::materialize_host(&data_dir.join("plugin-host")) {
        Ok(h) => h,
        Err(e) => {
            println!("  plugins:         FAIL to materialize host: {e:#}");
            return;
        }
    };
    let mut sc = match ocserve_plugin::Sidecar::spawn(&host, "http://127.0.0.1", "/").await {
        Ok(s) => s,
        Err(e) => {
            println!("  plugins:         FAIL to spawn host: {e:#}");
            return;
        }
    };
    let input = serde_json::json!({
        "directory": std::env::current_dir().map(|p| p.display().to_string()).unwrap_or_default(),
        "projectID": "global",
        "worktree": "/",
        "serverUrl": "http://127.0.0.1",
    });
    for (spec, options) in &specs {
        match ocserve_plugin::resolve_entry(spec, &home) {
            Ok(entry) => match sc.load(spec, &entry, &input, options).await {
                Ok(hooks) => {
                    println!("  plugin {spec}");
                    println!("    hooks:      {}", hooks.join(", "));
                    let inv = sc.v2_inventory().await.unwrap_or(serde_json::Value::Null);
                    if let Some(u) = inv.get("unbridged_transforms").and_then(|v| v.as_array())
                        && !u.is_empty()
                    {
                        let names: Vec<&str> = u.iter().filter_map(|v| v.as_str()).collect();
                        println!("    unbridged:  {}", names.join(", "));
                    }
                }
                Err(e) => println!("  plugin {spec}: FAIL ({e:#})"),
            },
            Err(e) => println!("  plugin {spec}: not found ({e})"),
        }
    }
    sc.shutdown().await;
}

fn import(source: std::path::PathBuf, limit: u32, data_dir: std::path::PathBuf) -> Result<()> {
    println!(
        "import {limit} sessions from {} into {}",
        source.display(),
        data_dir.display()
    );
    ocserve_importer::import_last_n(&source, &data_dir, limit)?;
    Ok(())
}
