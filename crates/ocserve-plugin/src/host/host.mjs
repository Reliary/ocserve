/**
 * ocserve plugin host (sidecar): NDJSON JSON-RPC on stdio.
 *
 * Why a sidecar (PLAN §10 gate, evidence-based): the frozen plugins import
 * `bun:sqlite`, `child_process`, `node:fs/crypto/url`, `process` and TTY UI
 * modules — a quickjs embed would need a dozen shims with a synchronous SQLite
 * bridge (deadlock-prone). Node v25 is present with `node:sqlite`; one host,
 * low maintenance. stdout is RESERVED for the protocol: all plugin output is
 * redirected to stderr so a chatty plugin cannot corrupt framing.
 */
import { pathToFileURL, fileURLToPath } from "node:url";
import { createRequire } from "node:module";
import {
  detectV2,
  v2Definition,
  setupV2,
  newV2State,
  triggerV2,
  makeV2Context,
  runTransforms,
} from "./v2.mjs";

// Runtime detection: under real Bun (upstream's runtime, preferred) the
// native surface exists — no shims needed. Under Node we install the Bun
// emulation: module hooks for `bun:sqlite` (node:sqlite-backed) and a
// truthy `globalThis.Bun` marker so plugins take their bun branches.
const IS_BUN = typeof globalThis.Bun !== "undefined" && typeof Bun.version === "string";
const hostRequire = createRequire(import.meta.url);

if (!IS_BUN) {
  const { register } = await import("node:module");
  const Module = (await import("node:module")).default;
  register("./bun-sqlite-loader.mjs", import.meta.url);
  const shimCjs = fileURLToPath(new URL("./shim-bun-sqlite.cjs", import.meta.url));
  const origResolve = Module._resolveFilename;
  Module._resolveFilename = function (request, ...rest) {
    if (request === "bun:sqlite") return shimCjs;
    return origResolve.call(this, request, ...rest);
  };
}
if (!IS_BUN) globalThis.Bun = Object.freeze({
  // Minimal marker: plugins branch on truthiness. Deliberately NO $ / sqlite /
  // filesystem surface — divergence documented in ocserve-plugin docs.
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
/** v2 plugins: {spec, id, dialect, cleanup} + shared dispatch state. */
const loadedV2 = [];
const v2State = newV2State();

function makeClient(serverUrl, directory, headers = {}) {
  // The SDK convention is `client.<ns>.<method>({ path, query, body })`, NOT
  // positional arguments. Treating the argument OBJECT as a body is what broke
  // Magic Context's historian: `session.messages({path:{id}})` became
  // `GET /session/messages` (404) and `session.create({path,query,body})` sent
  // the envelope as the payload. Measured 2026-10-22.
  //
  // Path segments are interpolated from `path` in declaration order, so
  // `{ path: { id } }` on `session.get` yields `/session/<id>`. `query` is
  // encoded; `body` is JSON. A bare string first argument is still accepted for
  // the legacy shorthand (`session.get(id)`).
  const call = (parts, method) => async (arg) => {
    let path = "/" + parts.map((p) => p.replace(/[A-Z]/g, (c) => "-" + c.toLowerCase())).join("/");
    let query = "";
    let body;
    if (typeof arg === "string") {
      path += "/" + encodeURIComponent(arg);
    } else if (arg && typeof arg === "object") {
      const { path: pathObj, query: queryObj, body: bodyObj } = arg;
      if (pathObj && typeof pathObj === "object") {
        if (Object.keys(pathObj).length > 0) {
          path += "/" + Object.values(pathObj).map((v) => encodeURIComponent(String(v))).join("/");
        }
      }
      if (queryObj && typeof queryObj === "object") {
        const qs = new URLSearchParams();
        for (const [k, v] of Object.entries(queryObj)) {
          if (v !== undefined && v !== null) qs.append(k, String(v));
        }
        const s = qs.toString();
        if (s) query = "?" + s;
      }
      if (bodyObj !== undefined) body = JSON.stringify(bodyObj);
    }
    const url = serverUrl.replace(/\/$/, "") + path + query;
    const res = await fetch(url, {
      method,
      headers: {
        "content-type": "application/json",
        "x-opencode-directory": directory,
        ...headers,
      },
      body: method === "GET" || method === "HEAD" ? undefined : body,
    });
    const text = await res.text();
    try {
      return JSON.parse(text);
    } catch {
      return text;
    }
  };
  const cache = new Map();
  return new Proxy(
    {},
    {
      get(_, ns) {
        if (typeof ns !== "string") return undefined;
        if (!cache.has(ns)) {
          cache.set(
            ns,
            new Proxy(
              {},
              {
                get(_, verbRaw) {
                  const verb = String(verbRaw);
                  if (!cache.has(ns + verb)) {
                    const method =
                      verb === "get" || verb === "list" || verb === "find" || verb === "search"
                        ? "GET"
                        : verb === "create" || verb === "post"
                          ? "POST"
                          : verb === "update" || verb === "patch"
                            ? "PATCH"
                            : verb === "delete" || verb === "remove"
                              ? "DELETE"
                              : "GET";
                    cache.set(ns + verb, call([ns, verb], method));
                  }
                  return cache.get(ns + verb);
                },
              },
            ),
          );
        }
        return cache.get(ns);
      },
    },
  );
}

// ---- v2 support helpers (Phase 3) -----------------------------------
/**
 * v2 storage: an in-memory per-plugin KV (upstream is disk-backed under the
 * plugin's data dir). Bounded and honest — a restart clears it (named
 * divergence D-V2-STORAGE-MEM). Keyed per plugin id.
 */
const v2Stores = new Map();
function makeV2Storage(pluginId) {
  if (!v2Stores.has(pluginId)) v2Stores.set(pluginId, new Map());
  const m = v2Stores.get(pluginId);
  return {
    get: async (key) => m.get(key),
    set: async (key, value) => {
      m.set(key, value);
    },
    remove: async (key) => {
      m.delete(key);
    },
    scan: async ({ prefix = "", after, limit = 100 } = {}) => {
      const keys = [...m.keys()].filter((k) => k.startsWith(prefix)).sort();
      const start = after ? keys.findIndex((k) => k > after) : 0;
      const slice = keys.slice(start < 0 ? 0 : start, (start < 0 ? 0 : start) + limit);
      const entries = slice.map((k) => ({ key: k, value: m.get(k) }));
      const next = entries.length === limit ? entries.at(-1).key : undefined;
      return next ? { entries, next } : { entries };
    },
  };
}

/**
 * Resolve a plugin's Effect runtime for effect-dialect plugins. We never
 * bundle Effect; the plugin's own node_modules must provide it. Resolve
 * relative to the plugin entry so `import("effect")` finds the copy the
 * plugin was installed with.
 */
function resolveEffectFrom(pluginId, entry) {
  return async () => {
    const require = createRequire(pathToFileURL(entry).href);
    let path;
    try {
      path = require.resolve("effect");
    } catch {
      return null;
    }
    const mod = await import(pathToFileURL(path).href);
    // effect exports { Effect, Scope, ... } as named; runPromise/scoped on Effect.
    return mod?.Effect ? mod : mod?.default?.Effect ? mod.default : null;
  };
}

async function loadPlugin({ spec, entry, origin, input, options }) {
  const mod = await import(pathToFileURL(entry).href);
  // v2 detection FIRST (V2-EXECUTION-PLAN Phase 3): a v2 `Plugin.define`
  // module carries {id, setup|effect}; v1 modules never do. Both dialects
  // normalize onto the shared v2 hook table + RPC registry.
  const dialect = detectV2(mod);
  if (dialect) {
    const def = v2Definition(mod);
    const serverUrl = input.serverUrl ?? "http://127.0.0.1";
    const directory = input.directory ?? "/";
    const client = makeClient(serverUrl, directory);
    const location = { directory };
    // Same request path as ctx.client, shaped for the shim's REST_METHODS
    // table: absolute path in, parsed JSON (or raw text) out.
    const fetchJson = async (method, path, body) => {
      const res = await fetch(serverUrl.replace(/\/$/, "") + path, {
        method,
        headers: { "content-type": "application/json", "x-opencode-directory": directory },
        body: body === undefined ? undefined : JSON.stringify(body),
      });
      const text = await res.text();
      try {
        return JSON.parse(text);
      } catch {
        return text;
      }
    };
    const ctx = makeV2Context({
      pluginId: def.id,
      options,
      hooks: v2State.hooks,
      rpcs: v2State.rpcs,
      storage: makeV2Storage(def.id),
      client,
      fetchJson,
      location,
      pluginList: () => loadedV2.map((p) => ({ id: p.id, spec: p.spec, dialect: p.dialect })),
      state: v2State,
    });
    // Register BEFORE setup so ctx.plugin.list() inside setup reports the
    // plugin being loaded (upstream registers the plugin, then initialises
    // it). On a setup failure the entry is removed again, so the inventory
    // never lists a plugin that failed to load.
    const rec = { spec, id: def.id, dialect, cleanup: null };
    loadedV2.push(rec);
    try {
      rec.cleanup = await setupV2(def, dialect, ctx, resolveEffectFrom(def.id, entry));
    } catch (e) {
      const i = loadedV2.indexOf(rec);
      if (i >= 0) loadedV2.splice(i, 1);
      throw e;
    }
    // Report the v2 hook names this plugin actually registered (observability).
    const names = [];
    for (const [domain, byName] of Object.entries(v2State.hooks)) {
      if (domain === "event") {
        if (byName.length) names.push("event");
        continue;
      }
      for (const [n, list] of Object.entries(byName)) if (list.length) names.push(`${domain}.${n}`);
    }
    return {
      hooks: names,
      id: def.id,
      dialect: "v2",
      unbridged_transforms: [...new Set(v2State.unbridgedTransforms)],
    };
  }
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
  loaded.push({ spec, hooks: merged, entry, origin: origin ?? entry });
  return { hooks: Object.keys(merged), id: mod?.PluginModule?.id ?? null };
}

function emitInfo(obj) {
  try { process.stdout.write(JSON.stringify(obj) + "\n"); } catch {}
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
      emitInfo({ event: "plugin_error", plugin: plugin.spec, hook: name, error: String(e?.message ?? e) });
    }
  }
  if (matched > 0) {
    // observability: prove dispatch happened (stderr → ocserve log)
    process.stderr.write(`[plugin-host] trigger ${name}: ${matched} hook(s)\n`);
  }
  return output;
}

/**
 * v1 `config` hook (plugin/index.ts:244-251): every loaded plugin's
 * `hooks.config(cfg)` is called once with the resolved config; each failure
 * logged + ignored (Effect.ignore). Distinct from `trigger` (which passes an
 * input+output pair) — here the single arg IS the config.
 */
async function dispatchConfig(cfg) {
  let matched = 0;
  for (const plugin of loaded) {
    const fn = plugin.hooks["config"];
    if (typeof fn !== "function") continue;
    matched++;
    try {
      await fn(cfg);
    } catch (e) {
      process.stderr.write(`[plugin:${plugin.spec}] config hook failed: ${e?.stack ?? e}\n`);
      emitInfo({ event: "plugin_error", plugin: plugin.spec, hook: "config", error: String(e?.message ?? e) });
    }
  }
  // Upstream hands the SAME object to every plugin and then derives from it
  // (config.ts). Returning only {matched} discarded every mutation — the
  // "called and ignored" class. The mutated config travels back to Rust.
  return { matched, config: cfg };
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


// ---- plugin tools (v1 `hooks.tool`) -------------------------------------
// Upstream converts the plugin's Zod args to JSON Schema in ITS process
// (registry.ts:125-183): `z.toJSONSchema` when every arg is Zod, else the
// legacy plain-object shape. We do the same here, using the PLUGIN'S OWN zod
// (resolved from its entry) — nothing is bundled or reimplemented.
function pluginZod(entry) {
  // The plugin may use Zod v3 or v4. Only v4's namespace exposes
  // `toJSONSchema`; resolve both and prefer the one that can convert. Asking
  // for `zod` alone resolved v3 here and silently fell back to a "legacy"
  // schema that embedded the raw Zod objects — an invalid JSON Schema the
  // provider rejects (live 400 on ctx_reduce, 2026-10-23).
  const req = createRequire(pathToFileURL(entry));
  for (const id of ["zod/v4", "zod"]) {
    try {
      const z = req(id);
      if (z && typeof z.toJSONSchema === "function") return z;
    } catch {
      /* try the next resolution */
    }
  }
  return null;
}

function legacySchema(args) {
  const props = {};
  for (const [k, v] of Object.entries(args ?? {})) {
    if (!v || typeof v !== "object" || Array.isArray(v)) continue;
    // A raw Zod type is NOT a JSON Schema. If we cannot convert, expose a
    // permissive schema instead of shipping `{~standard, def, type}` to the
    // provider — that is a hard 400 for every request in the session.
    props[k] = "_zod" in v ? {} : v;
  }
  return { type: "object", properties: props, required: Object.keys(props) };
}

function toParameters(def, entry) {
  const args = def?.args ?? {};
  const entries = Object.entries(args);
  const z = pluginZod(entry);
  const allZod = entries.length > 0 && entries.every(([, v]) => v && typeof v === "object" && "_zod" in v);
  if (allZod && z && typeof z.toJSONSchema === "function") {
    try {
      // `z.toJSONSchema` lives on the Zod NAMESPACE, not on the schema object
      // (a schema has `parse`/`safeParse` but no converter). Passing the raw
      // schemas through Object.fromEntries without this check produced
      // `{"drop":{"~standard":…,"def":…}}` — an invalid JSON Schema the
      // provider rejects with 400 (found live 2026-10-23: ctx_reduce).
      const json = z.toJSONSchema(z.object(args), { io: "input" });
      return normalizeZodJsonSchema(json, z);
    } catch (e) {
      process.stderr.write(`[plugin-host] z.toJSONSchema failed: ${e?.message ?? e}\n`);
    }
  }
  return legacySchema(args);
}

/**
 * Upstream's `normalizeZodJsonSchema` (registry.ts:409-421) plus the `$defs`
 * rename (registry.ts:376-380): drop boolean `exclusiveMaximum/Minimum` (not
 * valid in the 2020-12 dialect the provider enforces) and move draft-2020
 * `$defs` to the `definitions` key upstream sends.
 */
function normalizeZodJsonSchema(value) {
  if (Array.isArray(value)) return value.map((v) => normalizeZodJsonSchema(v));
  if (typeof value !== "object" || value === null) return value;
  const out = {};
  for (const [k, v] of Object.entries(value)) {
    if ((k === "exclusiveMaximum" || k === "exclusiveMinimum") && typeof v === "boolean") continue;
    out[k] = normalizeZodJsonSchema(v);
  }
  if (out.$defs && typeof out.$defs === "object" && !Array.isArray(out.$defs)) {
    out.definitions = out.$defs;
    delete out.$defs;
  }
  return out;
}

function collectTools() {
  const out = [];
  for (const p of loaded) {
    for (const [name, def] of Object.entries(p.hooks?.tool ?? {})) {
      if (!def || typeof def !== "object" || typeof def.execute !== "function") continue;
      out.push({
        name,
        description: String(def.description ?? ""),
        parameters: toParameters(def, p.origin ?? p.entry),
        plugin: p.spec,
      });
    }
  }
  return out;
}

async function executeTool({ name, input, ctx }) {
  for (const p of loaded) {
    const def = p.hooks?.tool?.[name];
    if (!def) continue;
    const controller = new AbortController();
    const toolCtx = {
      sessionID: ctx?.sessionID ?? "",
      messageID: ctx?.messageID ?? "",
      agent: ctx?.agent ?? "",
      directory: ctx?.directory ?? process.cwd(),
      worktree: ctx?.directory ?? process.cwd(),
      abort: controller.signal,
      // Authority stays in ocserve: the call was gated as a whole. A plugin
      // that gates its OWN dangerous operation behind `await ctx.ask(...)`
      // would otherwise proceed silently when we answer "yes" unconditionally
      // (review M-4). Refuse instead: the plugin fails closed, visibly, and
      // the operator sees a named error rather than an unapproved action.
      ask: async () => {
        throw new Error(
          "D-HOOK-TOOL-ASK: an intra-tool ask has no channel to the permission gate; refusing rather than approving silently",
        );
      },
      metadata: (m) => { toolCtx._meta = { ...(toolCtx._meta ?? {}), ...(m ?? {}) }; },
    };
    try {
      const r = await def.execute(input ?? {}, toolCtx);
      const value = typeof r === "string" ? { output: r } : (r ?? { output: "" });
      // Bound the payload HERE, before it crosses the NDJSON pipe: a 9 MiB
      // output overruns the reader's 8 MiB line cap and killed the reader for
      // good (review M-6). Rust truncates again for the model, but the wire
      // bound must hold at the producer.
      const LIMIT = 1024 * 1024;
      let output = typeof value.output === "string" ? value.output : String(value.output ?? "");
      let truncated = false;
      if (output.length > LIMIT) {
        output = output.slice(0, LIMIT);
        truncated = true;
      }
      // A plugin may return anything as metadata; only an object survives to
      // the part state (a non-object panicked the server, review H-1).
      const metadata =
        value.metadata && typeof value.metadata === "object" && !Array.isArray(value.metadata)
          ? value.metadata
          : (toolCtx._meta && typeof toolCtx._meta === "object" ? toolCtx._meta : {});
      return {
        ok: true,
        title: String(value.title ?? ""),
        output,
        truncated,
        metadata,
        attachments: value.attachments ?? undefined,
      };
    } catch (e) {
      return { ok: false, error: String(e?.message ?? e) };
    }
  }
  return { ok: false, error: `plugin tool not found: ${name}` };
}

/** Forward one bus event to every plugin's `event` hook (v1 + v2 shapes). */
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
  // v2 event.subscribe callbacks receive the raw event (EventDomain.subscribe).
  for (const cb of v2State.hooks.event ?? []) {
    try {
      await cb(ev);
      delivered++;
    } catch (e) {
      process.stderr.write(`[plugin:v2] event hook failed: ${e?.stack ?? e}\n`);
    }
  }
  return { delivered };
}

/**
 * Dispatch a v2 domain hook (Phase 3). Returns the mutated input (v2 hooks
 * mutate the event object in place, like v1's output). `matched` tells the
 * caller whether any plugin installed the hook.
 */
async function triggerV2Dispatch(domain, name, input) {
  const list = v2State.hooks[domain]?.[name] ?? [];
  let matched = list.length;
  const out = await triggerV2(v2State, domain, name, input);
  return { output: out, matched };
}

/**
 * Dispatch a registered plugin RPC. `rpcID` is the plugin-registered id; the
 * method must exist in its definition. Errors map to the v2 RPC error union
 * (method_not_found / unavailable); input is passed through as-is.
 */
async function callRpc(rpcID, method, input) {
  const reg = v2State.rpcs[rpcID];
  if (!reg) {
    return { error: { type: "rpc.unavailable", message: `RPC is unavailable: ${rpcID}` } };
  }
  const handler = reg.handlers?.[method];
  if (typeof handler !== "function") {
    return { error: { type: "rpc.method_not_found", message: `RPC method not found: ${rpcID}.${method}` } };
  }
  try {
    const signal = new AbortController().signal;
    const errorFactory = (e) => ({ type: "rpc.handler_error", message: String(e) });
    const result = await handler(input, { signal, error: errorFactory });
    return { output: result };
  } catch (e) {
    return { error: { type: "rpc.handler_error", message: String(e?.message ?? e) } };
  }
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
      // v2 domain hook dispatch (Phase 3): {domain, name, input} -> {output, matched}.
      case "trigger_v2":
        reply(await triggerV2Dispatch(msg.params.domain, msg.params.name, msg.params.input));
        break;
      // v2 RPC dispatch (Phase 3): {rpcID, method, input} -> {output}|{error}.
      case "rpc_call": reply(await callRpc(msg.params.rpcID, msg.params.method, msg.params.input)); break;
      // v2 plugin inventory (real, not fabricated): id + dialect + hook names.
      case "v2_inventory":
        reply({
          plugins: loadedV2.map((p) => ({ spec: p.spec, id: p.id, dialect: p.dialect })),
          rpcs: Object.keys(v2State.rpcs),
          unbridged_transforms: [...new Set(v2State.unbridgedTransforms)],
        });
        break;
      // v2 command/skill transform pull (Phase 3/Tier 2). Rust calls this on
      // config reload: it hands over the pristine command list, the plugin
      // side returns the edited one. PULLED, never pushed — a payload can
      // never serve a list from a reconcile that did not ask for it.
      case "v2_transform_commands":
        reply(await runTransforms(v2State, msg.params.commands));
        break;
      case "config": reply(await dispatchConfig(msg.params.config)); break;
      case "tools": reply(collectTools()); break;
      case "tool_execute": reply(await executeTool(msg.params)); break;
      // v1 parity (plugin/index.ts:255-259): per-plugin hooks["event"]
      // called with {event:{id,type,properties}}; each failure logged and
      // skipped (upstream: void fire-and-forget, fail-open).
      case "event": reply(await deliverEvent(msg.params.event)); break;
      case "dispose": {
        for (const p of loaded) { try { await p.hooks?.dispose?.(); } catch { /* documented: dispose errors logged, not fatal */ } }
        for (const p of loadedV2) { try { await p.cleanup?.(); } catch { /* v2 cleanup errors logged, not fatal */ } }
        reply("ok");
        process.exit(0);
      }
      default: fail(`unknown method: ${msg.method}`);
    }
  } catch (e) {
    fail(e?.stack ?? e);
  }
}
