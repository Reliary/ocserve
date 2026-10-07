//! refine CLI: serve, import, doctor, bench.

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(
    name = "refine",
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
        #[arg(long, default_value = "4901")]
        port: u16,
        #[arg(long, default_value = "127.0.0.1")]
        hostname: String,
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
    /// otherwise point at refine to check wire compat.
    Replay {
        /// e.g. http://127.0.0.1:4901 (upstream) or http://127.0.0.1:4911 (refine)
        #[arg(long)]
        target: String,
        /// tolerate routes the target doesn't implement yet (refine during M1-M3)
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

fn default_data_dir() -> std::path::PathBuf {
    std::env::var_os("REFINE_DATA_DIR")
        .map(Into::into)
        .unwrap_or_else(|| {
            let mut p =
                std::path::PathBuf::from(std::env::var_os("HOME").unwrap_or_else(|| ".".into()));
            p.push(".local/share/refine");
            p
        })
}

mod models_dev;
mod partition;
mod replay;
mod resilience;
mod runtime;

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("refine=info")),
        )
        .with_writer(std::io::stderr)
        .init();
    let cli = Cli::parse();
    match cli.cmd {
        Cmd::Serve {
            port,
            hostname,
            data_dir,
        } => rt::block_on(serve(
            hostname,
            port,
            data_dir.unwrap_or_else(default_data_dir),
        )),
        Cmd::Doctor { data_dir } => doctor(data_dir.unwrap_or_else(default_data_dir)),
        Cmd::Backup { dest, data_dir } => {
            let dir = data_dir.unwrap_or_else(default_data_dir);
            let db = refine_store::writer::db_path(&dir);
            // drill finding: VACUUM INTO on a READ_ONLY handle fails (SQLITE_READONLY);
            // open a writer conn (WAL + busy_timeout make this safe alongside a live server)
            let conn = refine_store::pragma::open_writer(&db)?;
            refine_store::backup_to(&conn, &dest)?;
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

async fn serve(hostname: String, port: u16, data_dir: std::path::PathBuf) -> Result<()> {
    use refine_http::{AppState, FREEZE_VERSION};

    std::fs::create_dir_all(&data_dir)
        .with_context(|| format!("create data dir {}", data_dir.display()))?;

    // fail-fast boot checks (SRE.md §1) — before binding
    refine_cli::boot_checks(&data_dir)?;

    // L2 partition (SRE §5): run BEFORE any child spawn (MCP probe / plugin
    // sidecar) so every wrapper can move itself into kids/. Scoped to
    // systemd invocations (INVOCATION_ID) — bare/test runs never restructure
    // a terminal's or harness's cgroup tree; REFINE_CGROUP_PARTITION=0/1 force.
    partition::setup();

    // Assemble config-derived payloads (fail fast if config unreadable)
    let rt = runtime::Runtime::load_for(&data_dir)
        .context("load runtime config (opencode.json/auth/models cache)")?;
    // M4a: probe MCP servers before serving (statuses captured, never fatal)
    let mcp_cfgs =
        refine_mcp::parse_config(rt.config.get("mcp").unwrap_or(&serde_json::Value::Null));
    let plugin_specs: Vec<String> = rt
        .config
        .get("plugin")
        .and_then(|v| v.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|s| s.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default();
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
    let payloads = rt.into_payloads(); // consumed after llm/mcp/plugins took their pieces
    let db_path = refine_store::writer::db_path(&data_dir);
    let writer = std::sync::Arc::new(
        refine_store::Writer::spawn(db_path.clone()).context("spawn store writer")?,
    );
    let blobs = std::sync::Arc::new(
        refine_store::BlobStore::new(data_dir.join("blobs")).context("blob store")?,
    );
    let writer_for_sampler = writer.clone();
    let wal_path_sampler = db_path.with_extension("db-wal");
    // W1 one-time search backfill (idempotent/resumable; ~7.5s measured
    // scale for126k parts; later boots no-op via the parity check)
    match refine_store::backfill_part_search(&writer, &db_path) {
        Ok((0, 0, _)) => {}
        Ok((idx, skip, ms)) => {
            tracing::info!("search backfill: {idx} indexed, {skip} skipped, {ms}ms");
            refine_metrics::counter("refine_search_backfilled_total", idx);
        }
        Err(e) => {
            tracing::warn!("search backfill failed (search incomplete until next boot): {e:#}")
        }
    }
    // M6: compaction projection backfill (idempotent; legacy imports may
    // carry upstream compaction anchors — COMPACTION.md §8.1 resolved)
    match refine_store::backfill_compaction(&writer, &db_path) {
        Ok((0, 0, _)) => {}
        Ok((anchors, links, ms)) => {
            tracing::info!("compaction backfill: {anchors} anchors, {links} summary links, {ms}ms");
        }
        Err(e) => tracing::warn!("compaction backfill failed (retries next boot): {e:#}"),
    }
    // W6: search parity gauge (part rows vs projection) — mismatch after
    // backfill = indexed drift worth seeing on /metrics
    if let Ok(conn) = refine_store::pragma::open_reader(&db_path) {
        let parts: i64 = conn
            .query_row("SELECT count(*) FROM msg_part", [], |r| r.get(0))
            .unwrap_or(-1);
        let indexed: i64 = conn
            .query_row("SELECT count(*) FROM part_search", [], |r| r.get(0))
            .unwrap_or(-1);
        refine_metrics::gauge("refine_search_indexed_rows", indexed.max(0));
        if parts >= 0 && indexed >= 0 && indexed != parts {
            tracing::warn!(
                "search index parity drift: {indexed}/{parts} parts indexed (backfill next boot)"
            );
        }
    }
    let state = AppState::with_wiring(
        None,
        payloads,
        refine_http::Wires {
            db: db_path,
            blobs,
            writer,
            llm,
        },
    );
    let hub = refine_mcp::McpHub::probe_all(&mcp_cfgs).await;
    // P2b: persist TOFU pins next to the DB so an offline rug-pull (server
    // edited while refine is down) trips drift at next boot
    if !mcp_cfgs.is_empty() {
        hub.set_pins_path(data_dir.join("mcp-pins.json"));
    }
    tracing::info!("mcp probe: {}", hub.statuses());
    refine_metrics::gauge(
        "refine_mcp_connected",
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
            dyn Fn() -> anyhow::Result<(refine_http::Payloads, refine_http::LlmRegistry)>
                + Send
                + Sync,
        > = std::sync::Arc::new(move || {
            let rt = crate::runtime::Runtime::load_for(&data_dir_for_reload)?;
            let registry = rt.llm_registry()?; // borrow first — into_payloads moves
            Ok((rt.into_payloads(), registry)) // move, not clone (measured OOM)
        });
        *state.reloader.write() = Some(reloader);
    }

    // H1: external-edit hot-reload — poll the exact files Runtime reads,
    // reconcile on change (fail-safe: broken config keeps old state serving)
    state.watch.write().paths = crate::runtime::watch_paths(&data_dir);
    refine_http::watch::observe(&state);
    // boot: hand the initial load pipeline's freed pages back (Tier-0 lab:
    // full catalog settles 237→91 MB with trim; tiny-catalog control = 19 MB)
    refine_http::watch::trim_heap();
    // periodic safety net: slow churn (sessions, searches) between reloads
    // still accumulates free-list pages; REFINE_TRIM_SECS=0 disables.
    if std::env::var("REFINE_TRIM").is_err() || std::env::var("REFINE_TRIM").as_deref() != Ok("0") {
        let trim_secs: u64 = std::env::var("REFINE_TRIM_SECS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(300);
        if trim_secs > 0 {
            tokio::spawn(async move {
                loop {
                    tokio::time::sleep(std::time::Duration::from_secs(trim_secs)).await;
                    refine_http::watch::trim_heap();
                }
            });
        }
    }
    tokio::spawn(refine_http::watch::watch_loop(
        state.clone(),
        refine_http::watch::WatchOpts::from_env(),
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

    // Legacy→refine delta sync (development bridge; kill switch
    // REFINE_LEGACY_SYNC=0). 60s cadence after a 5s settle, fail-soft: any
    // source error logs + counts, the task never dies.
    if refine_http::sync::sync_enabled() {
        let st = state.clone();
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_secs(5)).await;
            let mut tick = tokio::time::interval(std::time::Duration::from_secs(60));
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                tick.tick().await;
                // K-EFFICIENCY: tick cost visibility (the idle-jump suspect
                // until measured otherwise — 11 msgs total, so likely tiny).
                let sync_t0 = std::time::Instant::now();
                let tick_result = refine_http::sync::sync_tick(&st);
                refine_metrics::observe(
                    "refine_sync_tick",
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
                        refine_metrics::counter("refine_sync_messages_total", s.messages);
                        refine_metrics::gauge(
                            "refine_sync_last_epoch_seconds",
                            std::time::SystemTime::now()
                                .duration_since(std::time::UNIX_EPOCH)
                                .map(|d| d.as_secs() as i64)
                                .unwrap_or(0),
                        );
                    }
                    Err(e) => {
                        refine_metrics::counter("refine_sync_errors_total", 1);
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
            loop {
                tick.tick().await;
                n += 1;
                refine_metrics::sample_rss();
                refine_metrics::gauge("refine_writer_queue_depth", writer_m.queue_depth());
                let wal = std::fs::metadata(&wal_path)
                    .map(|m| m.len() as i64)
                    .unwrap_or(0);
                refine_metrics::gauge("refine_wal_bytes", wal);
                // MEMORY §7.6: DB growth KPI (thousands-of-sessions target —
                // gauged now, policy at ≥5 GB / ≥1k sessions).
                if let Some(db) = wal_path
                    .to_str()
                    .and_then(|w| w.strip_suffix("-wal"))
                    .and_then(|d| std::fs::metadata(d).ok())
                {
                    refine_metrics::gauge("refine_db_bytes", db.len() as i64);
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
                    refine_metrics::gauge("refine_sidecar_rss_bytes", r);
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
                        refine_metrics::labeled_counter(
                            "refine_sidecar_recycle_total",
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
                    refine_metrics::gauge("refine_mem_pressure_avg10", v);
                }

                // L2 kids accounting (only when the dance published one)
                if let Some(kp) = partition::kids_path()
                    && let Some(v) = std::fs::read_to_string(kp.join("memory.current"))
                        .ok()
                        .and_then(|s| s.trim().parse::<i64>().ok())
                {
                    refine_metrics::gauge("refine_kids_bytes", v);
                }
                refine_metrics::gauge("refine_prompt_locks", st.prompt_locks.lock().len() as i64);
                refine_metrics::gauge("refine_prompt_tasks", st.prompt_tasks.lock().len() as i64);
                refine_metrics::gauge(
                    "refine_question_pending",
                    st.question_gate.list().len() as i64,
                );
                refine_metrics::gauge("refine_permission_pending", st.gate.pending_len() as i64);
                if let Ok(c) = refine_store::session_count(&st.db) {
                    refine_metrics::gauge("refine_sessions_total", c);
                }
                // Storage maintenance (STORAGE §2 amendment: operational
                // pragmas run on the WRITER connection — config pragmas stay
                // in open()): drain freelist a bounded 4096 pages/tick (no-op
                // at freelist=0), refresh planner stats hourly. Never a
                // full VACUUM (online cost) — incremental only.
                let w = writer_m.clone();
                let optimize = n.is_multiple_of(240);
                let join = tokio::task::spawn_blocking(move || {
                    let mut ops = vec![refine_store::WriteOp::Sql {
                        sql: "PRAGMA incremental_vacuum(4096)".into(),
                        params: vec![],
                    }];
                    if optimize {
                        ops.push(refine_store::WriteOp::Sql {
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
            }
        });
    }

    // M4b: plugin sidecar — materialize host, spawn Node, load configured
    // plugins (statuses logged like /mcp; load failures never block serving)
    let mut plugin_sidecar: Option<refine_plugin::Sidecar> = None;
    if !plugin_specs.is_empty() {
        let host = refine_plugin::materialize_host(&data_dir.join("plugin-host"))
            .context("materialize plugin host")?;
        let home = std::path::PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| "/".into()));
        let server_url = format!("http://{hostname}:{port}");
        let directory = std::env::current_dir()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|_| "/".into());
        match refine_plugin::Sidecar::spawn(&host, &server_url, &directory).await {
            Ok(mut sc) => {
                // D1: normalization root (data-dir emit for self-contained
                // bundles; kill switch REFINE_PLUGIN_NORMALIZE=0)
                sc.set_normalize_root(data_dir.join("normalized"));
                let input = serde_json::json!({
                    "directory": directory,
                    "projectID": "global",
                    "worktree": "/",
                    "serverUrl": server_url,
                });
                for spec in &plugin_specs {
                    match refine_plugin::resolve_entry(spec, &home) {
                        Ok(entry) => match sc.load(spec, &entry, &input).await {
                            Ok(hooks) => tracing::info!("plugin {spec} loaded: {hooks:?}"),
                            Err(e) => tracing::warn!("plugin {spec} load failed: {e:#}"),
                        },
                        Err(e) => tracing::warn!("plugin {spec} resolve failed: {e:#}"),
                    }
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
        refine_http::start_plugin_event_pump(&state);
    }
    let app = refine_http::router(state.clone());

    let addr = format!("{hostname}:{port}");
    let listener = tokio::net::TcpListener::bind(&addr)
        .await
        .with_context(|| format!("bind {addr} (port in use?)"))?;
    tracing::info!("refine serving on http://{addr} (freeze {FREEZE_VERSION})");

    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await?;
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

async fn shutdown_signal() {
    let _ = tokio::signal::ctrl_c().await;
    tracing::info!("shutdown: draining");
}

fn doctor(data_dir: std::path::PathBuf) -> Result<()> {
    println!("refine doctor");
    println!("  sqlite version:  {}", rusqlite::version());
    match refine_store::pragma::assert_version_ok() {
        Ok(()) => println!(
            "  version gate:    OK (>= {})",
            refine_store::pragma::MIN_SQLITE_VERSION
        ),
        Err(e) => println!("  version gate:    FAIL: {e:#}"),
    }
    // The service is an OPTIONAL overlay — informational, never a failure
    // (foreground `refine serve` is the contract; preferred: scripts/install.sh).
    let unit = std::path::PathBuf::from(std::env::var("HOME").unwrap_or_default())
        .join(".config/systemd/user/refine.service");
    println!(
        "  service:        {}",
        if unit.exists() {
            "installed (optional overlay; scripts/uninstall.sh removes)"
        } else {
            "not installed (optional overlay; preferred: scripts/install.sh)"
        }
    );
    let db = refine_store::writer::db_path(&data_dir);
    if db.exists() {
        match refine_store::pragma::open_writer(&db) {
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
    Ok(())
}

fn import(source: std::path::PathBuf, limit: u32, data_dir: std::path::PathBuf) -> Result<()> {
    println!(
        "import {limit} sessions from {} into {}",
        source.display(),
        data_dir.display()
    );
    refine_importer::import_last_n(&source, &data_dir, limit)?;
    Ok(())
}
