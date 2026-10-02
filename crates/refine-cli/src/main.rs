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
    },
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

mod replay;
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
        Cmd::Import {
            source,
            limit,
            data_dir,
        } => import(source, limit, data_dir.unwrap_or_else(default_data_dir)),
        Cmd::Replay {
            target,
            allow_missing,
        } => rt::block_on(async move {
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
    }
}

mod rt {
    pub fn block_on<F: std::future::Future>(f: F) -> F::Output {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(8)
            .thread_stack_size(1 << 20)
            .max_blocking_threads(8)
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

    // Assemble config-derived payloads (fail fast if config unreadable)
    let rt = runtime::Runtime::load()
        .context("load runtime config (opencode.json/auth/models cache)")?;
    let payloads = refine_http::Payloads {
        config: rt.config,
        agent: rt.agent,
        api_agent: rt.api_agent,
        command: rt.command,
        config_providers: rt.config_providers,
        provider: rt.provider,
        console: rt.console,
        capabilities: rt.capabilities,
    };
    let state = AppState::with_payloads(None, payloads);
    // populate sessions from store (M1: boot snapshot; M2+ live updates)
    {
        let db = refine_store::writer::db_path(&data_dir);
        if db.exists() {
            match refine_store::load_sessions_wire(&db) {
                Ok(list) => {
                    let mut map = state.sessions.write();
                    for s in &list {
                        if let Some(id) = s.get("id").and_then(|v| v.as_str()) {
                            map.insert(id.to_string(), s.clone());
                        }
                    }
                    tracing::info!("loaded {} sessions from store", list.len());
                }
                Err(e) => tracing::warn!("session preload failed (continuing): {e:#}"),
            }
        }
    }
    let app = refine_http::router(state);

    let addr = format!("{hostname}:{port}");
    let listener = tokio::net::TcpListener::bind(&addr)
        .await
        .with_context(|| format!("bind {addr} (port in use?)"))?;
    tracing::info!("refine serving on http://{addr} (freeze {FREEZE_VERSION})");

    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await?;
    Ok(())
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
