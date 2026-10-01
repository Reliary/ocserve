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
    boot_checks(&data_dir)?;

    let state = AppState::new();
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

fn boot_checks(data_dir: &std::path::Path) -> Result<()> {
    use refine_store::pragma;
    // 1. version gate
    pragma::assert_version_ok().context("sqlite version gate")?;
    // 2. data dir writable
    let probe = data_dir.join(".boot-probe");
    std::fs::write(&probe, b"ok")
        .with_context(|| format!("write probe in {}", data_dir.display()))?;
    let _ = std::fs::remove_file(&probe);
    // 3. disk headroom > 2x WAL limit (64MB journal limit)
    match stat_free_bytes(data_dir) {
        Ok(free) if free < 128 * 1024 * 1024 => {
            anyhow::bail!("only {free} bytes free; need >128 MB (2x WAL limit)");
        }
        Ok(free) => tracing::debug!("disk headroom {free} bytes"),
        Err(e) => tracing::warn!("disk headroom check skipped: {e:#}"), // loud, not silent
    }
    // 4. open/create DB and run schema migration (creates pragmas profile)
    let db = refine_store::writer::db_path(data_dir);
    let conn = pragma::open_writer(&db).context("open/create database")?;
    refine_store::schema::migrate(&conn).context("schema migrate")?;
    // 5. FTS roundtrip on the real DB (catches broken builds — M0 finding)
    conn.execute_batch(
        "INSERT INTO search_doc (id, title, excerpt, updated_at) VALUES (-1, 'boot fts probe', '', 0);
         INSERT INTO search_fts(rowid, title, excerpt) SELECT id, title, excerpt FROM search_doc WHERE id = -1;
         DELETE FROM search_fts WHERE rowid = -1;
         DELETE FROM search_doc WHERE id = -1;",
    )
    .context("FTS5 roundtrip (broken FTS build?)")?;
    tracing::info!("boot checks OK");
    Ok(())
}

fn stat_free_bytes(p: &std::path::Path) -> Result<u64> {
    // Boot path only (not hot): shell out to `df -k`; avoids a libc dependency.
    let out = std::process::Command::new("df").arg("-k").arg(p).output()?;
    let text = String::from_utf8_lossy(&out.stdout);
    let line = text.lines().nth(1).context("df output")?;
    let avail: u64 = line
        .split_whitespace()
        .nth(3)
        .context("df avail column")?
        .parse()
        .context("df avail parse")?;
    Ok(avail * 1024)
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
