// Active probe: proves the plugin's OUTPUT is CONSUMED, not just dispatched.
//
// Each hook mutates in the documented way; the contract gate then looks for the
// mutations at their observable boundary (provider request, persisted tool part,
// shell child env). A hook whose return value we throw away passes a passive
// probe and fails this one — exactly the `tool.execute.after` / `config` class.
//
// Values are unique per run and greppable: "HOOKPROBE_<hook>".
import { appendFileSync } from "node:fs";

const LOG = process.env.HOOKS_LOG;
const TAG = process.env.HOOKS_TAG ?? "freeze";
const MAX = 200_000;
let seq = 0;

function clip(v) {
  try {
    const s = JSON.stringify(v ?? null);
    return s.length <= MAX ? v ?? null : { "<truncated>": true };
  } catch {
    return null;
  }
}

function record(hook, input, output) {
  if (!LOG) return;
  seq += 1;
  try {
    appendFileSync(
      LOG,
      JSON.stringify({ seq, tag: TAG, hook, pid: process.pid, input: clip(input), output: clip(output) }) + "\n",
    );
  } catch {}
}

export const PluginModule = {
  id: "hook-probe-active",
  server: async () => ({
    "chat.message": async (i, o) => {
      record("chat.message", i, o);
      if (Array.isArray(o?.parts)) {
        o.parts = o.parts.map((p) =>
          p?.type === "text" ? { ...p, text: `${p.text} HOOKPROBE_chat_message` } : p,
        );
      }
    },
    "chat.params": async (i, o) => {
      record("chat.params", i, o);
      o.temperature = 0.42;
      o.options = { ...(o.options ?? {}), hookprobe: true };
    },
    "chat.headers": async (i, o) => {
      record("chat.headers", i, o);
      o.headers = { ...(o.headers ?? {}), "x-hookprobe": "1" };
    },
    "experimental.chat.system.transform": async (i, o) => {
      record("experimental.chat.system.transform", i, o);
      o.system = [...(o.system ?? []), "HOOKPROBE_system"];
    },
    "experimental.chat.messages.transform": async (i, o) => {
      record("experimental.chat.messages.transform", i, o);
      if (Array.isArray(o?.messages)) {
        // length-changing on purpose: the old compaction guard rejected exactly
        // this, and the prompt path used to receive a shape the plugin could not
        // read at all.
        o.messages.push({
          info: { role: "user", sessionID: o.messages[0]?.info?.sessionID ?? "ses_probe" },
          parts: [{ type: "text", text: "HOOKPROBE_messages" }],
        });
      }
    },
    "experimental.session.compacting": async (i, o) => {
      record("experimental.session.compacting", i, o);
      o.context = [...(o.context ?? []), "HOOKPROBE_compacting"];
    },
    "experimental.text.complete": async (i, o) => {
      record("experimental.text.complete", i, o);
      o.text = `${o.text ?? ""} HOOKPROBE_text`;
    },
    "shell.env": async (i, o) => {
      record("shell.env", i, o);
      o.env = { ...(o.env ?? {}), HOOKPROBE_SHELL_ENV: "1" };
    },
    "tool.execute.before": async (i, o) => {
      record("tool.execute.before", i, o);
      if (o?.args && typeof o.args === "object") o.args.hookprobe = true;
    },
    "tool.execute.after": async (i, o) => {
      record("tool.execute.after", i, o);
      o.title = "HOOKPROBE_title";
      o.metadata = { ...(o.metadata ?? {}), hookprobe: true };
      o.output = `${o.output ?? ""} HOOKPROBE_after`;
    },
    "tool.definition": async (i, o) => {
      record("tool.definition", i, o);
      if (i?.toolID === "bash") o.description = `${o.description ?? ""} HOOKPROBE_definition`;
    },
    "command.execute.before": async (i, o) => {
      record("command.execute.before", i, o);
      if (Array.isArray(o?.parts)) {
        o.parts = o.parts.map((p) =>
          p?.type === "text" ? { ...p, text: `${p.text} HOOKPROBE_command` } : p,
        );
      }
    },
  }),
};
export default PluginModule.server;

// `tool.execute.after`'s output is only observable where it lands: the persisted
// part and the next provider request. The active probe appends a unique marker,
// and the gate greps the session's stored parts for it.
