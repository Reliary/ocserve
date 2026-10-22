#!/usr/bin/env node
/**
 * Real-plugin compatibility check for ocserve.
 *
 * ## What it measures
 *
 * For every pinned package in `manifest.json`:
 *   1. resolve the entry file the way ocserve's Rust loader does
 *      (`exports["."]` → `main` → `module`),
 *   2. load it through the **real** sidecar host (`host.mjs`), sending the same
 *      NDJSON `load` request the server sends,
 *   3. record the hooks the plugin registered,
 *   4. invoke each registered v1 hook with a synthesized payload and record
 *      whether it ran, threw, or mutated the output,
 *   5. deliver one synthetic bus event through the v1 `event` channel.
 *
 * ## What it deliberately does NOT measure
 *
 * Whether the hook's *effect* is useful. That needs a model turn and a real
 * session; this reports mechanical compatibility (loads, registers, runs)
 * and names the classes it cannot judge. A plugin that loads and registers
 * is not thereby "compatible" — see the report legend.
 *
 * ## Safety
 *
 * Plugins are arbitrary code. This script assumes it is running inside the
 * sandbox (`bench/plugins/sandbox.sh`): no network, non-root, tmpfs HOME,
 * bounded time/memory. It does not attempt to sandbox by itself.
 *
 * Usage:
 *   node check.mjs --dir <packages-dir> [--out report.json] [--only name]
 *     packages-dir contains one subdirectory per package, named by package
 *     name with `/` replaced by `__`.
 */
import { spawn } from "node:child_process";
import { createInterface } from "node:readline";
import fs from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";

const HERE = path.dirname(fileURLToPath(import.meta.url));
const ROOT = path.resolve(HERE, "../..");
const HOST = path.join(ROOT, "crates/ocserve-plugin/src/host/host.mjs");

const V1_HOOKS = [
  "chat.message",
  "chat.params",
  "chat.headers",
  "experimental.chat.messages.transform",
  "experimental.chat.system.transform",
  "experimental.text.complete",
  "experimental.session.compacting",
  "tool.execute.before",
  "tool.execute.after",
  "command.execute.before",
];

/** Synthesized payloads: (input, output). Kept minimal but shape-plausible. */
const SAMPLES = {
  "chat.message": [{ sessionID: "ses_check", messageID: "msg_check", agent: "build" }, { parts: [] }],
  "chat.params": [{ sessionID: "ses_check", provider: "stub", model: "stub-1" }, { temperature: 0 }],
  "chat.headers": [{ sessionID: "ses_check", provider: "stub", model: "stub-1" }, {}],
  "experimental.chat.messages.transform": [{}, { messages: [{ info: { role: "user", sessionID: "ses_check" }, parts: [] }] }],
  "experimental.chat.system.transform": [{ sessionID: "ses_check" }, { system: [] }],
  "experimental.text.complete": [{ sessionID: "ses_check" }, { text: "" }],
  "experimental.session.compacting": [{ sessionID: "ses_check" }, { context: [] }],
  "tool.execute.before": [{ tool: "bash", sessionID: "ses_check", callID: "call_check" }, { args: {} }],
  "tool.execute.after": [{ tool: "bash", sessionID: "ses_check", callID: "call_check" }, { output: "" }],
  "command.execute.before": [{ command: "init", sessionID: "ses_check" }, { arguments: {} }],
};

function entryFor(pkgDir) {
  const pj = JSON.parse(fs.readFileSync(path.join(pkgDir, "package.json"), "utf8"));
  let rel;
  const dot = pj.exports && typeof pj.exports === "object" ? pj.exports["."] : undefined;
  // Mirror the Rust loader exactly: only a STRING import/default counts. A
  // nested condition object (types + default) is not an entry, and the loader
  // falls through to main/module. Node would resolve it; ocserve does not, so
  // the harness must measure ocserve's rule, not Node's.
  if (typeof dot === "string") rel = dot;
  else if (dot && typeof dot === "object") {
    const v = dot.import ?? dot.default;
    if (typeof v === "string") rel = v;
  }
  rel = rel ?? (typeof pj.main === "string" ? pj.main : undefined);
  rel = rel ?? (typeof pj.module === "string" ? pj.module : undefined);
  if (!rel) return { error: "no exports/main/module", pj };
  const entry = path.resolve(pkgDir, rel);
  if (!entry.startsWith(path.resolve(pkgDir) + path.sep) && entry !== path.resolve(pkgDir)) {
    return { error: `entry escapes package: ${rel}`, pj };
  }
  if (!fs.existsSync(entry)) return { error: `entry missing: ${rel}`, pj };
  return { entry, pj };
}

class Sidecar {
  constructor() {
    this.child = spawn(process.execPath, [HOST], {
      stdio: ["pipe", "pipe", "pipe"],
      cwd: path.join(ROOT, "crates/ocserve-plugin/src/host"),
      env: { ...process.env, HOME: process.env.HOME ?? "/tmp", NO_COLOR: "1" },
    });
    this.next = 1;
    this.pending = new Map();
    this.stderr = "";
    this.child.stderr.on("data", (d) => {
      this.stderr += d.toString();
      if (this.stderr.length > 64 * 1024) this.stderr = this.stderr.slice(-32 * 1024);
    });
    const rl = createInterface({ input: this.child.stdout });
    rl.on("line", (line) => {
      let msg;
      try {
        msg = JSON.parse(line);
      } catch {
        return;
      }
      const p = this.pending.get(msg.id);
      if (!p) return;
      this.pending.delete(msg.id);
      if (msg.error) p.reject(new Error(msg.error.message ?? "sidecar error"));
      else p.resolve(msg.result);
    });
  }

  request(method, params, timeoutMs = 15000) {
    const id = this.next++;
    const line = JSON.stringify({ id, method, params }) + "\n";
    return new Promise((resolve, reject) => {
      const t = setTimeout(() => {
        this.pending.delete(id);
        reject(new Error(`timeout after ${timeoutMs}ms`));
      }, timeoutMs);
      this.pending.set(id, {
        resolve: (v) => {
          clearTimeout(t);
          resolve(v);
        },
        reject: (e) => {
          clearTimeout(t);
          reject(e);
        },
      });
      this.child.stdin.write(line);
    });
  }

  kill() {
    try {
      this.child.kill("SIGKILL");
    } catch {}
  }
}

async function checkOne(sc, item, pkgDir) {
  const rec = {
    name: item.name,
    version: item.version,
    declared: null,
    entry: null,
    status: "unknown",
    dialect: null,
    hooks: [],
    invoked: {},
    event: null,
    error: null,
  };
  const e = entryFor(pkgDir);
  rec.declared = e.pj
    ? {
        sdk_v1: Boolean(
          (e.pj.dependencies ?? {})["@opencode-ai/plugin"] ||
            (e.pj.peerDependencies ?? {})["@opencode-ai/plugin"],
        ),
        sdk_v2: Boolean(
          (e.pj.dependencies ?? {})["@opencode/plugin"] ||
            (e.pj.peerDependencies ?? {})["@opencode/plugin"],
        ),
      }
    : null;
  if (e.error) {
    rec.status = "entry-error";
    rec.error = e.error;
    return rec;
  }
  rec.entry = path.relative(pkgDir, e.entry);
  let load;
  try {
    load = await sc.request(
      "load",
      {
        spec: `${item.name}@${item.version}`,
        entry: e.entry,
        input: {
          directory: "/work",
          worktree: "/work",
          project: { id: "check", directory: "/work" },
          serverUrl: "http://127.0.0.1:9",
          $: undefined,
        },
      },
      20000,
    );
  } catch (err) {
    const msg = String(err.message ?? err);
    if (/Cannot find (package|module)/.test(msg) || msg.includes("ResolveMessage")) {
      rec.status = "dependency";
      rec.error = msg.slice(0, 400);
      return rec;
    }
    if (msg.includes("timeout")) {
      // The sandbox has no network. A plugin that awaits a fetch during setup
      // looks identical to a slow one, so this is named as a timeout rather
      // than guessed as "needs network".
      rec.status = "timeout";
      rec.error = `${msg} (network is off in the sandbox)`;
    } else {
      rec.status = "load-error";
      rec.error = msg.slice(0, 400);
    }
    return rec;
  }
  rec.status = "loaded";
  rec.dialect = load?.dialect ?? "v1";
  rec.hooks = Array.isArray(load?.hooks) ? load.hooks : [];

  if (rec.dialect === "v2") {
    // v2 hooks are dispatched by domain; the load result already reports the
    // registered names. Invoking them needs shape-specific inputs and is not
    // part of this mechanical pass.
    rec.invoked = { skipped: "v2 dispatch is domain-typed; registration only" };
    return rec;
  }

  for (const name of rec.hooks) {
    if (!V1_HOOKS.includes(name)) continue;
    const [input, output] = SAMPLES[name] ?? [{}, {}];
    try {
      const res = await sc.request("trigger", { name, input, output }, 10000);
      const changed = JSON.stringify(res) !== JSON.stringify(output);
      rec.invoked[name] = changed ? "ran+mutated" : "ran";
    } catch (err) {
      rec.invoked[name] = `threw: ${String(err.message ?? err).slice(0, 160)}`;
    }
  }

  if (rec.hooks.includes("config")) {
    try {
      const res = await sc.request(
        "config",
        { config: { $schema: "https://opencode.ai/config.json", model: "stub/stub-1" } },
        10000,
      );
      rec.config = res && typeof res === "object" ? "ran" : "ran";
    } catch (err) {
      rec.config = `threw: ${String(err.message ?? err).slice(0, 160)}`;
    }
  }

  if (rec.hooks.includes("event")) {
    try {
      const res = await sc.request(
        "event",
        { event: { id: "evt_check", type: "session.idle", properties: { sessionID: "ses_check" } } },
        10000,
      );
      rec.event = res?.delivered > 0 ? "delivered" : "registered-not-delivered";
    } catch (err) {
      rec.event = `threw: ${String(err.message ?? err).slice(0, 160)}`;
    }
  }
  return rec;
}

function classify(rec) {
  if (rec.status === "entry-error") return "entry";
  if (rec.status === "timeout") return "load-timeout";
  if (rec.status === "dependency") return "missing-dependency";
  if (rec.status === "load-error") return "load-error";
  if (rec.dialect === "v2") return "v2-registered";
  const v1 = Object.keys(rec.invoked ?? {}).filter((k) => k.includes("."));
  if (v1.length === 0) return rec.hooks.length ? "registered-only" : "loaded-no-hooks";
  if (v1.some((k) => String(rec.invoked[k]).startsWith("threw"))) return "hook-threw";
  return "hooks-ran";
}

async function main() {
  const args = process.argv.slice(2);
  let dir = null;
  let out = null;
  let only = null;
  for (let i = 0; i < args.length; i++) {
    if (args[i] === "--dir") dir = args[++i];
    else if (args[i] === "--out") out = args[++i];
    else if (args[i] === "--only") only = args[++i];
  }
  if (!dir) {
    console.error("usage: check.mjs --dir <packages-dir> [--out report.json] [--only name]");
    process.exit(2);
  }
  const manifest = JSON.parse(fs.readFileSync(path.join(HERE, "manifest.json"), "utf8")).plugins;
  const results = [];
  for (const item of manifest) {
    if (only && item.name !== only) continue;
    const pkgDir = path.join(dir, item.name.replaceAll("/", "__"));
    if (!fs.existsSync(pkgDir)) {
      results.push({ name: item.name, version: item.version, status: "missing", error: "not fetched" });
      continue;
    }
    // Fresh sidecar per plugin: a plugin that corrupts the host state or hangs
    // must not affect the next result. Also bounds memory per plugin.
    const sc = new Sidecar();
    try {
      const rec = await checkOne(sc, item, pkgDir);
      rec.class = classify(rec);
      results.push(rec);
    } finally {
      sc.kill();
    }
  }
  const summary = {};
  for (const r of results) summary[r.class ?? r.status] = (summary[r.class ?? r.status] ?? 0) + 1;
  const report = {
    generated_at: new Date().toISOString(),
    node: process.version,
    host: HOST.replace(ROOT + "/", ""),
    corpus: manifest.length,
    summary,
    results,
  };
  const text = JSON.stringify(report, null, 2);
  if (out) fs.writeFileSync(out, text);
  else process.stdout.write(text + "\n");
  console.error(`summary: ${JSON.stringify(summary)}`);
}

main().catch((e) => {
  console.error(e);
  process.exit(1);
});
