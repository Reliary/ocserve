// Passive hook probe: records every trigger the reference implementation fires.
//
// Loaded from an absolute path (freeze classifies absolute specs as `file` and
// imports them directly), so no npm install and no dependency tree.
//
// Writes one JSON object per line to $HOOKS_LOG:
//   {"seq":N,"tag":"freeze","hook":"...","input":{...},"output":{...}}
//
// It NEVER mutates: a probe that changes behaviour cannot measure behaviour.
// Values are truncated — this is a corpus, not a data store.
import { appendFileSync } from "node:fs";

const LOG = process.env.HOOKS_LOG;
const TAG = process.env.HOOKS_TAG ?? "freeze";
const MAX = 200_000;
let seq = 0;

function safe(value) {
  try {
    return JSON.stringify(value ?? null);
  } catch (e) {
    return JSON.stringify({ "<unserializable>": String(e?.message ?? e) });
  }
}

function clip(value) {
  const s = safe(value);
  if (s.length <= MAX) return value === undefined ? null : value;
  return { "<truncated>": true, bytes: s.length };
}

function record(hook, input, output) {
  if (!LOG) return;
  seq += 1;
  try {
    appendFileSync(
      LOG,
      JSON.stringify({
        seq,
        tag: TAG,
        hook,
        pid: process.pid,
        input: clip(input),
        output: clip(output),
      }) + "\n",
    );
  } catch {
    /* a probe must never break the plugin host */
  }
}

// The full declared v1 trigger set. A hook we do not register is a hook the host
// will never call, so the corpus would silently lack it.
const TRIGGERS = [
  "chat.message",
  "chat.params",
  "chat.headers",
  "command.execute.before",
  "experimental.chat.messages.transform",
  "experimental.chat.system.transform",
  "experimental.session.compacting",
  "experimental.text.complete",
  "shell.env",
  "tool.execute.before",
  "tool.execute.after",
  "tool.definition",
];

export const PluginModule = {
  id: "hook-probe",
  server: async () => {
    const hooks = {};
    for (const name of TRIGGERS) {
      hooks[name] = async (input, output) => {
        record(name, input, output);
      };
    }
    // `event` and `config` are single-argument hooks with no output object.
    hooks.event = async ({ event }) => {
      record("event", { type: event?.type, payload: event?.payload }, null);
    };
    hooks.config = async (cfg) => {
      record("config", cfg, null);
    };
    return hooks;
  },
};
export default PluginModule.server;
