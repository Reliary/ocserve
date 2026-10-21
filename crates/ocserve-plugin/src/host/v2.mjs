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
 * REST-backed domain methods — the v2 `Pick<XApi, ...>` half of a domain,
 * bound ONLY where the ocserve route actually exists at that path.
 *
 * Why an explicit table and not the generic `client` proxy: the proxy derives
 * paths as `{ns}/{verb}`, which is not how these routes are spelled
 * (`permission.saved`, not `permission.list`). Binding a method to a path we
 * have not verified would hand a plugin a 404 where upstream works — the
 * "shape without mutation" class. Anything absent here keeps the named
 * divergence (D-V2-DOMAIN) instead.
 *
 * `fetchJson` is the host-provided request function (same one `ctx.client`
 * uses): `fetchJson(method, path, body)`.
 */
const REST_METHODS = {
  session: {
    get: { path: (id) => `/api/session/${enc(id)}` },
    message: { path: (id) => `/api/session/${enc(id)}/message` },
    context: { path: (id) => `/api/session/${enc(id)}/context` },
    history: { path: (id) => `/api/session/${enc(id)}/history` },
    inbox: { path: (id) => `/api/session/${enc(id)}/inbox` },
    diff: { path: (id) => `/api/session/${enc(id)}/diff` },
  },
  permission: {
    saved: { path: () => "/api/permission/saved" },
    request: { path: () => "/api/permission/request" },
  },
};

/**
 * Build the REST half of a domain from REST_METHODS + the fetch function.
 *
 * Returns a PLAIN OBJECT with an own accessor per known verb, not a bare
 * Proxy: object spread (`{ ...restDomain(...) }`) enumerates own keys, and a
 * key-less Proxy spreads to nothing — which silently produced the very
 * `ctx.session.get is not a function` this table exists to fix.
 */
function restDomain(name, fetchJson) {
  const table = REST_METHODS[name] ?? {};
  const out = {};
  for (const verb of Object.keys(table)) {
    Object.defineProperty(out, verb, {
      enumerable: true,
      value: (...args) => fetchJson("GET", table[verb].path(args[0]), args[1]),
    });
  }
  // Unknown verbs keep the honest named divergence.
  return new Proxy(out, {
    get(target, prop) {
      if (prop in target || typeof prop !== "string" || prop === "then") return target[prop];
      return () => {
        throw new Error(`D-V2-DOMAIN: ctx.${name}.${prop} has no bound ocserve route`);
      };
    },
  });
}

function enc(v) {
  return encodeURIComponent(String(v ?? ""));
}

/** Merge a domain's hook registrar with its bound REST half.
 *
 * The Proxy must survive as the object's PROTOTYPE chain holder, not be
 * spread: spreading copies own keys and drops the trap, so unknown verbs
 * became `undefined` (a TypeError at the call site) instead of the named
 * divergence. We copy the REST verbs as own props onto a fresh object whose
 * lookups fall through to the Proxy's trap for everything else.
 */
function mergeDomain(registrarObj, name, fetchJson) {
  const rest = restDomain(name, fetchJson);
  const merged = Object.create(rest); // unknown verbs → rest's throwing trap
  Object.assign(merged, registrarObj); // hook/... own props win
  // REST verbs must also be reachable as own props for `in`/spread users.
  for (const verb of Object.keys(REST_METHODS[name] ?? {})) {
    Object.defineProperty(merged, verb, {
      enumerable: true,
      value: rest[verb],
    });
  }
  return merged;
}

/**
 * Build the v2 `ctx` (Context, promise/plugin.ts) over the host's dispatch
 * tables. `hooks` is the shared v2 hook table; `rpcs` is the RPC registry;
 * `storage` is the storage backend; `client` issues REST calls.
 */
export function makeV2Context({
  pluginId,
  options,
  hooks,
  rpcs,
  storage,
  client,
  fetchJson,
  location,
  app,
  pluginList,
  state,
}) {
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
    // hook registrar + the bound REST half (see REST_METHODS).
    session: mergeDomain(registrar("session"), "session", fetchJson),
    tool: {
      ...registrar("tool"),
      // transform/list/reload operate on the tool registry — not bridged
      // (named divergence D-V2-TOOL-EDITOR); present so detection does not
      // crash, but a call throws the named error rather than lying.
      // Registered, never run: failing `setup` here would lose the plugin's
      // execute.before/after hooks, which ARE bridged. D-V2-TOOL-EDITOR.
      transform: async (callback) => {
        state.toolTransforms.push(callback);
        state.unbridgedTransforms.push("tool");
        return registration(state.toolTransforms, callback);
      },
      list: async () => [],
      reload: async () => {},
    },
    permission: mergeDomain(registrar("permission"), "permission", fetchJson),
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
    aisdk: unbridgedDomain("aisdk", state, []),
    model: unbridgedDomain("model", state, []),
    provider: unbridgedDomain("provider", state, []),
    agent: unbridgedDomain("agent", state, []),
    // command: real. `transform` collects CommandEditor callbacks; Rust pulls
    // `runTransforms` on config reload and stores what the plugins return.
    command: {
      // upstream `CommandDomain.transform` (command.ts) — the ONLY editor
      // verb upstream ships is `add` (core/command.ts:61), so a plugin can
      // contribute a command but not rewrite or drop an existing one.
      transform: (callback) => {
        state.commandTransforms.push(callback);
        return registration(state.commandTransforms, callback);
      },
      list: async () => state.lastCommands,
      reload: async () => {
        // Rust re-runs the pull after this resolves, so the editor callbacks
        // always run in the reconcile window (option A: never push).
        state.reloadRequested = true;
      },
    },
    // skill: real. Same underlying vector — `skill_entries` filters
    // Payloads.command on source=="skill", so command + skill transforms
    // compose over one list and both consumers see the result.
    skill: {
      transform: (callback) => {
        state.skillTransforms.push(callback);
        return registration(state.skillTransforms, callback);
      },
      list: async () => state.lastCommands.filter((c) => c && c.source === "skill"),
      get: async (id) => state.lastCommands.find((c) => c && c.source === "skill" && c.name === id),
      reload: async () => {
        state.reloadRequested = true;
      },
    },
    integration: unbridgedDomain("integration", state, []),
    mcp: unbridgedDomain("mcp", state, []),
    reference: unbridgedDomain("reference", state, []),
    vcs: unbridgedDomain("vcs", state, []),
    websearch: unbridgedDomain("websearch", state, []),
    worktree: unbridgedDomain("worktree", state, []),
    generate: unbridgedDomain("generate", state, []),
    // `experimental.terminal.read` is a real verb we do not implement; keep it
    // throwing on use (there is no registration form for it).
    experimental: {
      terminal: {
        read: () => {
          throw new Error("D-V2-DOMAIN: ctx.experimental.terminal.read is not bridged in ocserve");
        },
      },
    },
    // Real inventory, not []. `/api/plugin` serves the loaded list, so a
    // plugin asking here must see the same thing — an empty array would
    // contradict a closed divergence and the route it mirrors.
    plugin: { list: async () => (pluginList ? pluginList() : []) },
  };
}

/**
 * A domain whose edit surface is not bridged.
 *
 * `transform` MUST exist and MUST NOT throw at registration time: v2 plugins
 * register transforms during `setup`, and a throw there fails the whole plugin
 * — losing every other hook it registered, including the ones ocserve does
 * bridge. So the transform is accepted, recorded as unbridged, and never
 * invoked. The editor callback is never called, which is the honest behaviour
 * (the plugin's edit does not happen); the divergence is reported at boot from
 * the pending list rather than by killing the plugin.
 *
 * Any *other* property is genuine use of an unbridged domain and throws the
 * named divergence, as before.
 */
function unbridgedDomain(name, state, pending) {
  return new Proxy(
    {},
    {
      get: (_, prop) => {
        if (prop === "then") return undefined;
        if (prop === "transform") {
          return async (callback) => {
            // Recorded, not run. `pending` is reported so the operator sees a
            // plugin asked for an edit ocserve cannot apply.
            pending.push({ domain: name, callback });
            if (state && Array.isArray(state.unbridgedTransforms)) {
              state.unbridgedTransforms.push(name);
            }
            return registration(pending, callback);
          };
        }
        if (prop === "reload") {
          // `reload` asks the host to re-run transforms and refresh. Since no
          // transform of this domain is ever applied, there is nothing to
          // refresh: resolving is honest (it asserts no change) and keeps the
          // plugin's OTHER capabilities alive. A plugin that calls
          // transform → reload → list is common; failing on `reload` would
          // discard hooks ocserve does bridge, for no gain.
          return async () => {};
        }
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
  return {
    hooks,
    rpcs: {},
    // v2 command/skill transforms (command.ts CommandEditor, skill.ts
    // SkillEditor). Rust PULLS these on config reload — the plugin never
    // pushes an edit, so a payload can never serve a list from a reconcile
    // that did not ask for it.
    commandTransforms: [],
    skillTransforms: [],
    // Transforms for domains we do not bridge: recorded so the plugin's setup
    // succeeds, never invoked. `unbridgedTransforms` is the report.
    toolTransforms: [],
    unbridgedTransforms: [],
    // what the last pull produced; backs ctx.command.list()/skill.list().
    lastCommands: [],
    reloadRequested: false,
  };
}

/**
 * Run every registered command/skill transform over `list`.
 *
 * Mirrors upstream `State.create` (core/state.ts:172-194): transforms start
 * from the PRISTINE list each time, run in registration order, and a throwing
 * transform is disabled for this run and reported — never fatal, because a
 * broken third-party plugin must not take the server's command list with it.
 *
 * Returns `{ commands, failures }` so the caller can log every failure.
 */
export async function runTransforms(state, list) {
  const out = Array.isArray(list) ? [...list] : [];
  const failures = [];
  for (const [kind, cbs] of [
    ["command", state.commandTransforms],
    ["skill", state.skillTransforms],
  ]) {
    for (let i = 0; i < cbs.length; i++) {
      try {
        await cbs[i]({
          add: (def) => {
            if (!def || typeof def.name !== "string" || def.name === "") return;
            const existing = out.findIndex((c) => c && c.name === def.name);
            // upstream `editor.set(name, definition)` (core/command.ts:61) —
            // add REPLACES a same-named entry, so a plugin re-run is idempotent.
            const entry = {
              name: def.name,
              description: typeof def.description === "string" ? def.description : "",
              source: kind === "skill" ? "skill" : (def.source ?? "plugin"),
              // `execute` is a closure — it cannot cross the wire, so the
              // definition records that it was plugin-provided (named, not
              // silently absent: execution routes are v1-owned).
              pluginProvided: true,
            };
            if (existing >= 0) out[existing] = entry;
            else out.push(entry);
          },
          // skill.ts SkillEditor verbs; command.ts ships only `add`, so these
          // exist on the skill editor alone (same object, kind-checked).
          list: () => out.filter((c) => c && c.source === "skill"),
          get: (id) => out.find((c) => c && c.source === "skill" && c.name === id),
          update: (id, mutate) => {
            const hit = out.find((c) => c && c.source === "skill" && c.name === id);
            if (hit) mutate(hit);
          },
          remove: (id) => {
            const i = out.findIndex((c) => c && c.name === id);
            if (i >= 0) out.splice(i, 1);
          },
        });
      } catch (cause) {
        failures.push({ kind, index: i, message: String((cause && cause.message) || cause) });
      }
    }
  }
  state.lastCommands = out;
  return { commands: out, failures };
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
