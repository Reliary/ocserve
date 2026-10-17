/**
 * ocserve v2 plugin shim (V2-EXECUTION-PLAN Phase 3).
 *
 * Speaks BOTH v2 registration dialects — promise (`Plugin.define({id, setup})`)
 * and effect (`Plugin.define({id, effect})`) — and normalizes them onto ONE
 * dispatch path so the Rust side has a single v2 hook table + RPC registry.
 *
 * Design (D1/D7): never bundle or re-implement Effect. The promise dialect is
 * called directly; the effect dialect is run through the `effect` runtime
 * resolved from the PLUGIN's own context (real Effect, real scope semantics).
 * If a plugin's effect runtime is unreachable we fail loudly (never silently
 * drop the plugin).
 *
 * What is bridged vs. named-diverged is documented in
 * crates/ocserve-plugin/src/host/README-v2.md and asserted by the v2 battery.
 */

/** v2 hook names we dispatch, grouped by domain (mirrors promise/*.ts). */
const V2_HOOK_NAMES = {
  session: [
    "prompt",
    "context",
    "compaction",
    "generate",
    "title",
    "model.request",
    "http.request",
    "http.response",
    "retry",
  ],
  tool: ["execute.before", "execute.after"],
  permission: ["evaluate"],
  shell: ["create.before"],
};

/** A registration handle that removes one callback (v2 `Registration`). */
function registration(list, cb) {
  let done = false;
  return {
    dispose: async () => {
      if (done) return;
      done = true;
      const i = list.indexOf(cb);
      if (i >= 0) list.splice(i, 1);
    },
  };
}

/**
 * Build the v2 `ctx` (Context, promise/plugin.ts) over the host's dispatch
 * tables. `hooks` is the shared v2 hook table; `rpcs` is the RPC registry;
 * `storage` is the storage backend; `client` issues REST calls.
 */
export function makeV2Context({ pluginId, options, hooks, rpcs, storage, client, location, app }) {
  const registrar = (domain) => ({
    hook: async (name, cb, _opts) => {
      if (!V2_HOOK_NAMES[domain] || !V2_HOOK_NAMES[domain].includes(name))
        throw new Error(`unknown ${domain} hook: ${name}`);
      const list = (hooks[domain][name] ||= []);
      list.push(cb);
      return registration(list, cb);
    },
  });
  return {
    app: app || { name: "ocserve", version: "0.3.1", channel: "local" },
    location,
    options: options ?? {},
    session: { ...registrar("session") },
    tool: {
      ...registrar("tool"),
      // transform/list/reload operate on the tool registry — not bridged
      // (named divergence D-V2-TOOL-EDITOR); present so detection does not
      // crash, but a call throws the named error rather than lying.
      transform: () => {
        throw new Error("D-V2-TOOL-EDITOR: ctx.tool.transform is not bridged in ocserve");
      },
      list: async () => [],
      reload: async () => {},
    },
    permission: { ...registrar("permission") },
    shell: { ...registrar("shell") },
    // event.subscribe: the host pushes bus events to it (same delivery as v1).
    event: {
      subscribe: async (cb) => {
        const list = (hooks.event ||= []);
        list.push(cb);
        return registration(list, cb);
      },
    },
    // RPC registration — the novel surface (/api/rpc/{id}/{method}).
    rpc: {
      register: async (definition, handlers) => {
        const id = definition?.id;
        if (typeof id !== "string" || !id)
          throw new Error("ctx.rpc.register: definition.id is required");
        // last-wins (registration.ts: later registrations replace earlier).
        rpcs[id] = { id, definition, handlers, pluginId };
        return {
          dispose: async () => {
            if (rpcs[id] && rpcs[id].pluginId === pluginId) delete rpcs[id];
          },
          events: {
            emit: async () => {
              throw new Error("D-V2-RPC-EVENTS: rpc registration events are not bridged");
            },
          },
        };
      },
    },
    storage,
    // client: method-path REST proxy (same shape the v1 shim exposes).
    client,
    // Domains with no ocserve backing: exposing them as honest throwing
    // proxies (named divergences) beats undefined-property crashes at setup.
    aisdk: notBridged("aisdk"),
    model: notBridged("model"),
    provider: notBridged("provider"),
    agent: notBridged("agent"),
    command: notBridged("command"),
    integration: notBridged("integration"),
    mcp: notBridged("mcp"),
    reference: notBridged("reference"),
    skill: notBridged("skill"),
    vcs: notBridged("vcs"),
    websearch: notBridged("websearch"),
    worktree: notBridged("worktree"),
    generate: notBridged("generate"),
    experimental: { terminal: { read: () => notBridged("experimental.terminal").read() } },
    plugin: { list: async () => [] },
  };
}

/** A domain proxy whose async methods throw a named divergence on use. */
function notBridged(name) {
  return new Proxy(
    {},
    {
      get: (_, prop) => {
        if (prop === "then") return undefined;
        return () => {
          throw new Error(`D-V2-DOMAIN: ctx.${name}.${String(prop)} is not bridged in ocserve`);
        };
      },
    },
  );
}

/** REST namespace proxy: client.session.get(id) -> GET {base}/session/{id}. */
export function makeClient(call) {
  return new Proxy(
    {},
    {
      get: (_, ns) => (typeof ns === "string" ? makeClientNs(call, ns) : undefined),
    },
  );
}

function makeClientNs(call, ns) {
  const cache = new Map();
  return new Proxy(
    {},
    {
      get: (_, verbRaw) => {
        const verb = String(verbRaw);
        if (!cache.has(verb)) cache.set(verb, (...args) => call(ns, verb, ...args));
        return cache.get(verb);
      },
    },
  );
}

/**
 * Detect a v2 plugin module. Returns "promise" | "effect" | null.
 * v2 `Plugin.define` yields an object with `id` + (`setup` | `effect`).
 */
export function detectV2(mod) {
  const d = mod?.default;
  if (d && typeof d === "object" && typeof d.id === "string") {
    if (typeof d.setup === "function") return "promise";
    if (typeof d.effect === "function") return "effect";
  }
  // named export form: `export const plugin = Plugin.define(...)`.
  for (const v of Object.values(mod ?? {})) {
    if (v && typeof v === "object" && typeof v.id === "string") {
      if (typeof v.setup === "function") return "promise";
      if (typeof v.effect === "function") return "effect";
    }
  }
  return null;
}

/** The `Plugin.define` payload from a module (default, else first matching). */
export function v2Definition(mod) {
  const d = mod?.default;
  if (d && typeof d === "object" && typeof d.id === "string") return d;
  for (const v of Object.values(mod ?? {})) {
    if (v && typeof v === "object" && typeof v.id === "string") return v;
  }
  return null;
}

/**
 * Run a v2 plugin's setup/effect with the given context.
 * promise: `await def.setup(ctx)` → optional cleanup fn.
 * effect:  `def.effect(ctx)` → an Effect; run via the plugin's own runtime.
 */
export async function setupV2(def, dialect, ctx, resolveEffect) {
  if (dialect === "promise") {
    const cleanup = await def.setup(ctx);
    return typeof cleanup === "function" ? cleanup : null;
  }
  // effect dialect: obtain the Effect runtime the plugin depends on.
  const resolved = await resolveEffect(def.id);
  // `effect` exports the Effect namespace as a named export; accept either the
  // namespace itself or a module wrapping it.
  const Effect = resolved?.Effect ?? resolved;
  if (!Effect || typeof Effect.runPromise !== "function") {
    throw new Error(
      `plugin ${def.id}: effect-dialect plugin but no 'effect' runtime reachable from its context`,
    );
  }
  const program = def.effect(ctx);
  // Scope requirement (Plugin<R = Scope.Scope>): run scoped so resources a
  // plugin acquires are released when the sidecar disposes.
  const runnable =
    Effect.scoped && Effect.Scope ? Effect.scoped(program) : program;
  await Effect.runPromise(runnable);
  return null; // effect plugins manage their own Scope; no returned cleanup
}

/** Fresh (per-load) v2 state: hook table + RPC registry. */
export function newV2State() {
  const hooks = { event: [] };
  for (const [domain, names] of Object.entries(V2_HOOK_NAMES)) {
    hooks[domain] = {};
    for (const n of names) hooks[domain][n] = [];
  }
  return { hooks, rpcs: {} };
}

/** Fire a v2 domain hook; sequential, await, fail-open (logged by caller). */
export async function triggerV2(state, domain, name, input) {
  const list = state.hooks[domain]?.[name];
  if (!list || list.length === 0) return input;
  let matched = 0;
  for (const cb of list) {
    matched++;
    await cb(input);
  }
  return input;
}
