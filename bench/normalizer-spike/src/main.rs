//! R3 spike: normalize an opencode plugin entry into ONE self-contained ESM
//! file, report what the bundle did with each edge.
//!
//! Kill criteria: ../README.md (pre-registered before this file existed).
//!
//! Design answers (README attack ledger):
//!   - output MUST sit beside the entry: magic-context does
//!     `await import(`@huggingface/${"transformers"}`)` — computed dynamic
//!     import no bundler can analyze → stays runtime-resolved → needs the
//!     entry's node_modules ancestry.
//!   - treeshake OFF: top-level side effects ARE the registration contract.
//!   - bun:sqlite aliased onto refine's node:sqlite shim (inlined as module).
//!   - node builtins external (platform: node); everything else bundled.

use anyhow::{bail, Context, Result};
use rolldown::plugin::{
    HookLoadArgs, HookLoadOutput, HookResolveIdArgs, HookResolveIdOutput, HookUsage, Plugin,
    PluginContext, SharedLoadPluginContext,
};
use rolldown::{
    Bundler, BundlerOptions, InputItem, LogLevel, OutputFormat, Platform, TreeshakeOptions,
};
use std::borrow::Cow;
use std::future::Future;
use std::path::{Path, PathBuf};

/// Real refine shim — build-time include so drift is impossible.
const SHIM_PATH: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../crates/refine-plugin/src/host/shim-bun-sqlite.mjs"
);
const SHIM_ID: &str = "refine:bun-sqlite-shim";

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("normalize") => {
            let entry = args
                .get(1)
                .context("usage: normalize <entry> [--out p]")?
                .clone();
            let out = match args.iter().position(|a| a == "--out") {
                Some(i) => PathBuf::from(args.get(i + 1).context("--out needs value")?),
                None => default_out(&entry),
            };
            let t = std::time::Instant::now();
            // K6 cache: content-hash (entry + shim bytes + tool version) in
            // a sidecar .hash file — warm hit skips rolldown entirely.
            let shim_path =
                std::env::var("NORMALIZER_SHIM").unwrap_or_else(|_| SHIM_PATH.to_string());
            let h = content_hash(Path::new(&entry), Path::new(&shim_path))?;
            let hash_file = out.with_extension("mjs.hash");
            let warm = std::fs::read_to_string(&hash_file)
                .map(|old| old.trim() == h)
                .unwrap_or(false)
                && out.exists();
            if warm {
                println!("WARM HIT {} (hash {h})", out.display());
                return Ok(());
            }
            normalize(Path::new(&entry), &out)?;
            std::fs::write(&hash_file, &h)?;
            println!("OK {} in {:.0}ms", out.display(), t.elapsed().as_millis());
        }
        _ => bail!("usage: normalize <entry> [--out p]"),
    }
    Ok(())
}

/// `<dir>/<stem>.normalized.mjs` — beside the entry (A1: dynamic-import ancestry).
fn default_out(entry: &str) -> PathBuf {
    let p = Path::new(entry);
    let stem = p.file_stem().unwrap_or_default().to_string_lossy();
    p.with_file_name(format!("{stem}.normalized.mjs"))
}

fn normalize(entry: &Path, out: &Path) -> Result<()> {
    // NORMALIZER_SHIM env override = K5b experiment (spike variant shim);
    // default = refine's shipped shim so the experiment is explicit.
    let shim_path = std::env::var("NORMALIZER_SHIM").unwrap_or_else(|_| SHIM_PATH.to_string());
    let shim_src =
        std::fs::read_to_string(&shim_path).with_context(|| format!("read shim {shim_path}"))?;
    let entry = entry
        .canonicalize()
        .with_context(|| format!("canonicalize {}", entry.display()))?;

    let opts = BundlerOptions {
        input: Some(vec![InputItem {
            name: Some("plugin".to_string()),
            import: entry.to_string_lossy().into_owned(),
        }]),
        cwd: Some(entry.parent().unwrap_or(Path::new(".")).to_path_buf()),
        format: Some(OutputFormat::Esm),
        platform: Some(Platform::Node),
        // K7: side effects are the contract — never treeshake them away.
        treeshake: TreeshakeOptions::Boolean(false),
        log_level: Some(LogLevel::Info),
        ..Default::default()
    };

    let plugin = BunSqliteAlias {
        shim_src,
        shim_id: SHIM_ID.to_string(),
    };
    let mut bundler = Bundler::with_plugins(
        opts,
        vec![rolldown::plugin::__inner::Pluginable::new_shared(plugin)],
    )
    .map_err(|e| anyhow::anyhow!("bundler init: {e:?}"))?;

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .context("tokio rt")?;
    let output = rt
        .block_on(bundler.generate())
        .map_err(|e| anyhow::anyhow!("generate: {e:?}"))?;

    for w in &output.warnings {
        println!("warning: {w:?}");
    }

    let mut chunks = Vec::new();
    for asset in &output.assets {
        if let rolldown_common::Output::Chunk(chunk) = asset {
            chunks.push(chunk.clone());
        }
    }
    if chunks.len() != 1 {
        bail!(
            "K1 FAIL: expected exactly 1 chunk, got {} ({:?})",
            chunks.len(),
            chunks
                .iter()
                .map(|c| c.filename.as_str())
                .collect::<Vec<_>>()
        );
    }
    let chunk = &chunks[0];
    std::fs::write(out, &chunk.code).with_context(|| format!("write {}", out.display()))?;
    println!(
        "chunk {} ({} bytes, {} modules)",
        out.display(),
        chunk.code.len(),
        chunk.module_ids.len()
    );
    Ok(())
}

/// Maps bare specifier `bun:sqlite` onto a virtual module holding the shim
/// (which imports `node:sqlite` — kept external by platform: node).
#[derive(Debug)]
struct BunSqliteAlias {
    shim_src: String,
    shim_id: String,
}

impl Plugin for BunSqliteAlias {
    fn name(&self) -> Cow<'static, str> {
        Cow::Borrowed("refine-bun-sqlite-alias")
    }

    fn resolve_id(
        &self,
        _ctx: &PluginContext,
        args: &HookResolveIdArgs<'_>,
    ) -> impl Future<Output = rolldown::plugin::HookResolveIdReturn> + Send {
        let hit = args.specifier == "bun:sqlite";
        // Guard: the K5b shim's OWN `import("bun:"+"sqlite")` may be
        // constant-folded back to a literal — it must stay a RUNTIME import
        // (catchable rejection off-bun), never aliased to itself.
        let from_shim = args.importer == Some(self.shim_id.as_str());
        let id = self.shim_id.clone();
        async move {
            if hit && from_shim {
                Ok(Some(rolldown::plugin::HookResolveIdOutput {
                    id: "bun:sqlite".into(),
                    external: Some(rolldown_common::ResolvedExternal::Bool(true)),
                    ..Default::default()
                }))
            } else if hit {
                Ok(Some(HookResolveIdOutput::from_id(id)))
            } else {
                Ok(None)
            }
        }
    }

    fn load(
        &self,
        _ctx: SharedLoadPluginContext,
        args: &HookLoadArgs<'_>,
    ) -> impl Future<Output = rolldown::plugin::HookLoadReturn> + Send {
        let hit = args.id == self.shim_id;
        let code = self.shim_src.clone();
        async move {
            if hit {
                Ok(Some(HookLoadOutput {
                    code: code.into(),
                    map: None,
                    side_effects: None,
                    module_type: Some(rolldown_common::ModuleType::Js),
                }))
            } else {
                Ok(None)
            }
        }
    }

    fn register_hook_usage(&self) -> HookUsage {
        HookUsage::ResolveId | HookUsage::Load
    }
}

fn content_hash(entry: &Path, shim: &Path) -> Result<String> {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(std::fs::read(entry).with_context(|| format!("read {}", entry.display()))?);
    h.update(std::fs::read(shim).with_context(|| format!("read {}", shim.display()))?);
    h.update(b"normalizer-spike/v1"); // tool-version salt: bump invalidates
    Ok(format!("{:x}", h.finalize()))
}
