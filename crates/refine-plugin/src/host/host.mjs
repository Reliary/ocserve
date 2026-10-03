/**
 * refine plugin host (sidecar): NDJSON JSON-RPC on stdio.
 *
 * Why a sidecar (PLAN §10 gate, evidence-based): the frozen plugins import
 * `bun:sqlite`, `child_process`, `node:fs/crypto/url`, `process` and TTY UI
 * modules — a quickjs embed would need a dozen shims with a synchronous SQLite
 * bridge (deadlock-prone). Node v25 is present with `node:sqlite`; one host,
 * low maintenance. stdout is RESERVED for the protocol: all plugin output is
 * redirected to stderr so a chatty plugin cannot corrupt framing.
 */
import { pathToFileURL, fileURLToPath } from "node:url";
import { register, createRequire } from "node:module";
import Module from "node:module";

register("./bun-sqlite-loader.mjs", import.meta.url);

// CJS path for `require("bun:sqlite")` (context-mode does this when
// globalThis.Bun exists) + truthy Bun so plugins take their bun branches
// (they were written for the Bun runtime upstream runs under).
const hostRequire = createRequire(import.meta.url);
const shimCjs = fileURLToPath(new URL("./shim-bun-sqlite.cjs", import.meta.url));
const origResolve = Module._resolveFilename;
Module._resolveFilename = function (request, ...rest) {
  if (request === "bun:sqlite") return shimCjs;
  return origResolve.call(this, request, ...rest);
};
globalThis.Bun = Object.freeze({
  // Minimal marker: plugins branch on truthiness. Deliberately NO $ / sqlite /
  // filesystem surface — divergence documented in refine-plugin docs.
  // `hash`: real Bun.hash surface used by plugins (magic-context
  // directoryFallback: Bun.hash(p).toString(16) → stable project id). FNV-1a
  // double-pass — deterministic per input across runs (ids need run-stability,
  // never cross-runtime equality with Bun's wyhash). Missing entirely crashed
  // the fallback path live (TypeError → plugin load failed on non-git cwd).
  hash(value) {
    const s = typeof value === "string" ? value : String(value);
    let a = 0x811c9dc5;
    let b = 0x9e3779b9;
    for (let i = 0; i < s.length; i++) {
      const c = s.charCodeAt(i);
      a = Math.imul(a ^ c, 0x01000193) >>> 0;
      b = Math.imul(b ^ (c + i), 0x85ebca6b) >>> 0;
    }
    return a * 65536 + (b >>> 16); // < 2^48: clean hex, exact in doubles
  },
  // Bun.CryptoHasher surface (magic-context system.transform: md5+hex) —
  // node:crypto is the host runtime's own hasher, so algorithms match Node's
  // list rather than Bun's (md5/sha* covered; the only algorithms the real
  // plugin set constructs — dist grep: Bun.CryptoHasher ×2, Bun.hash ×1).
  CryptoHasher: class CryptoHasher {
    constructor(algo) {
      this.h = hostRequire("node:crypto").createHash(algo);
    }
    update(data) {
      this.h.update(data);
      return this;
    }
    digest(encoding) {
      return this.h.digest(encoding || "hex");
    }
  },
});

// ---- stdout discipline: protocol only -------------------------------
const realWrite = process.stdout.write.bind(process.stdout);
process.stdout.write = (chunk, ...rest) => process.stderr.write(chunk, ...rest);

// ---- plugin registry -------------------------------------------------
/** @type {{spec: string, hooks: Record<string, Function>}[]} */
const loaded = [];

function makeClient(serverUrl, directory, headers = {}) {
  // Generic fetch-backed client: method path → HTTP (upper camel → kebab),
  // e.g. client.session.get(id) → GET {serverUrl}/session/{id}. Plugins that
  // only need directory/project never touch it (reliary8).
  const call = (parts, body, method) => async (...args) => {
    let path = "/" + parts.map((p) => p.replace(/[A-Z]/g, (c) => "-" + c.toLowerCase())).join("/");
    let query = "";
    if (method === "GET" && typeof args[0] === "string" && parts.at(-1) === "get") {
      path = path.replace(/\/get$/, "/" + encodeURIComponent(args[0]));
      args = [];
    }
    if (typeof args[0] === "object" && args[0] && method !== "GET") {
      body = JSON.stringify(args[0]);
      args = [];
    }
    const url = serverUrl.replace(/\/$/, "") + path + query;
    const res = await fetch(url, {
      method,
      headers: { "content-type": "application/json", "x-opencode-directory": directory, ...headers },
      body: body ?? undefined,
    });
    const text = await res.text();
    try { return JSON.parse(text); } catch { return text; }
  };
  const cache = new Map();
  return new Proxy({}, {
    get(_, ns) {
      if (typeof ns !== "string") return undefined;
      if (!cache.has(ns)) {
        cache.set(ns, new Proxy({}, {
          get(_, verbRaw) {
            const verb = String(verbRaw);
            if (!cache.has(ns + verb)) {
              const method = verb === "get" || verb === "list" || verb === "find" || verb === "search" ? "GET" : verb === "create" || verb === "post" ? "POST" : verb === "update" || verb === "patch" ? "PATCH" : verb === "delete" || verb === "remove" ? "DELETE" : "GET";
              cache.set(ns + verb, call([ns, verb], undefined, method));
            }
            return cache.get(ns + verb);
          },
        }));
      }
      return cache.get(ns);
    },
  });
}

async function loadPlugin({ spec, entry, input, options }) {
  const mod = await import(pathToFileURL(entry).href);
  // v1 extraction (plugin/index.ts readV1Plugin + getLegacyPlugins), in order:
  // PluginModule.server → default object's .server → default function (legacy)
  // → named plain-function exports (each IS a server factory)
  const isRecord = (v) => v !== null && typeof v === "object" && !Array.isArray(v);
  let factories = [];
  if (typeof mod?.PluginModule?.server === "function") {
    factories.push(mod.PluginModule.server);
  } else if (isRecord(mod?.default) && typeof mod.default.server === "function") {
    factories.push(mod.default.server);
  } else if (typeof mod?.default === "function") {
    factories.push(mod.default);
  } else {
    for (const [key, value] of Object.entries(mod ?? {})) {
      if (key === "default") continue;
      if (typeof value !== "function") continue;
      if (value.prototype instanceof Object && value.prototype.constructor === value && value.prototype.constructor.name !== value.name) {
        // class export → skip (getLegacyPlugins skips classes)
        if (/^[A-Z]/.test(key)) continue;
      }
      if (/^[A-Z]/.test(key) && value.prototype && value.prototype.constructor === value) continue;
      factories.push(value);
    }
    if (factories.length === 0) {
      throw new Error(`plugin ${spec}: no server factory (default/PluginModule/named functions)`);
    }
  }
  const fullInput = {
    ...input,
    client: makeClient(input.serverUrl ?? "http://127.0.0.1", input.directory ?? "/"),
    get serverUrl() { return new URL(input.serverUrl ?? "http://127.0.0.1"); },
    experimental_workspace: { register() {} },
    $: undefined, // Bun.$ absent by design (documented divergence)
  };
  const merged = {};
  for (const factory of factories) {
    const hooks = await factory(fullInput, options);
    if (!hooks || typeof hooks !== "object") throw new Error(`plugin ${spec}: server() did not return hooks`);
    Object.assign(merged, hooks);
  }
  loaded.push({ spec, hooks: merged });
  return { hooks: Object.keys(merged), id: mod?.PluginModule?.id ?? null };
}

async function trigger(name, input, output) {
  // v1 semantics (Plugin.trigger): sequential, await fn(input, output),
  // return output (hooks mutate output in place).
  let matched = 0;
  for (const plugin of loaded) {
    const fn = plugin.hooks[name];
    if (typeof fn !== "function") continue;
    matched++;
    try {
      await fn(input, output);
    } catch (e) {
      process.stderr.write(`[plugin:${plugin.spec}] ${name} hook failed: ${e?.stack ?? e}\n`);
    }
  }
  if (matched > 0) {
    // observability: prove dispatch happened (stderr → refine log)
    process.stderr.write(`[plugin-host] trigger ${name}: ${matched} hook(s)\n`);
  }
  return output;
}

// ---- protocol loop ---------------------------------------------------
let buffer = "";
process.stdin.setEncoding("utf8");
process.stdin.on("data", (chunk) => {
  buffer += chunk;
  let idx;
  while ((idx = buffer.indexOf("\n")) >= 0) {
    const line = buffer.slice(0, idx).trim();
    buffer = buffer.slice(idx + 1);
    if (!line) continue;
    handle(line).catch((e) => process.stderr.write(`[host] handler error: ${e?.stack ?? e}\n`));
  }
});

/** Forward one bus event to every plugin's `event` hook (v1 shape). */
async function deliverEvent(ev) {
  let delivered = 0;
  for (const p of loaded) {
    const fn = p.hooks["event"];
    if (typeof fn !== "function") continue;
    try {
      await fn({ event: ev });
      delivered++;
    } catch (e) {
      process.stderr.write(`[plugin:${p.spec}] event hook failed: ${e?.stack ?? e}\n`);
    }
  }
  return { delivered };
}

async function handle(line) {
  let msg;
  try { msg = JSON.parse(line); } catch { return; }
  const reply = (result) => realWrite(JSON.stringify({ id: msg.id, result }) + "\n");
  const fail = (message) => realWrite(JSON.stringify({ id: msg.id, error: { message: String(message) } }) + "\n");
  try {
    switch (msg.method) {
      case "ping": reply("pong"); break;
      case "load": reply(await loadPlugin(msg.params)); break;
      case "trigger": reply(await trigger(msg.params.name, msg.params.input, msg.params.output)); break;
      case "config": reply("ok"); break; // config() hooks: invoked at load-time parity, M4b
      // v1 parity (plugin/index.ts:255-259): per-plugin hooks["event"]
      // called with {event:{id,type,properties}}; each failure logged and
      // skipped (upstream: void fire-and-forget, fail-open).
      case "event": reply(await deliverEvent(msg.params.event)); break;
      case "dispose": {
        for (const p of loaded) { try { await p.hooks?.dispose?.(); } catch { /* documented: dispose errors logged, not fatal */ } }
        reply("ok");
        process.exit(0);
      }
      default: fail(`unknown method: ${msg.method}`);
    }
  } catch (e) {
    fail(e?.stack ?? e);
  }
}
