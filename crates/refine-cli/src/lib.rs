//! refine-cli library surface: boot self-checks (SRE §1) exposed for tests.

use anyhow::{Context, Result};

pub fn boot_checks(data_dir: &std::path::Path) -> Result<()> {
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
    // 4.5 ring bound enforced at boot (STORAGE §4; AGENTS §2.3)
    let pruned = refine_store::enforce_event_retention(&conn).context("event retention")?;
    if pruned > 0 {
        tracing::info!("event retention at boot: {pruned} rows pruned");
    }
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

pub fn stat_free_bytes(p: &std::path::Path) -> Result<u64> {
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
