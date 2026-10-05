// Phase-1 probes: faithful mirror of refine's provider wire against zen/v1.
// One request per invocation (A5 discipline). Stages:
//   P1  refine wire as-is: content-type + accept:text/event-stream +
//       x-opencode-session (refine's native 26-hex), UA reqwest/0.12.28,
//       body {model, messages, stream, stream_options, max_tokens,
//       tool_choice:auto, tools = refine schemas()}   → does current refine pass?
//   P2  P1 + x-opencode-client/request/project         → which headers load-bearing
//   P3  P2 + composite UA (last-resort impersonation)
//   P4a P1 headers, body WITHOUT tools                 → compaction/tools-off path
//   P4b P1 headers, body tools: []
//   P4c P1 headers, real tools + tool_choice: none
const crypto = require("crypto");

const stage = (process.argv[2] || "P1").toUpperCase();

// refine ids.rs: prefix + 26 chars (13-hex time + 13-hex) — gate-format proxy
const hex26 = () => crypto.randomBytes(13).toString("hex");
const sid = "ses_" + hex26();
const mid = "msg_" + hex26();

const REFINE_UA = "reqwest/0.12.28";
const COMPOSITE_UA = "opencode/1.18.31 ai-sdk/provider-utils/4.0.23 runtime/bun/1.3.14";

const headers = {
  "content-type": "application/json",
  accept: "text/event-stream",
  "x-opencode-session": sid,
  "user-agent": REFINE_UA,
};
if (stage === "P2" || stage === "P3") {
  headers["x-opencode-client"] = "cli";
  headers["x-opencode-request"] = mid;
  headers["x-opencode-project"] = "global";
}
if (stage === "P3") headers["user-agent"] = COMPOSITE_UA;
// P3b: UA only (no client/request/project) — is the UA alone the header key?
if (stage === "P3B") headers["user-agent"] = COMPOSITE_UA;
// P4*: tools axis measured under the PROVEN header set (P3's), not P1's
// P5/P6: EXACT refine wire incl. bearer_auth("public") — the port target.
if (stage === "P5" || stage === "P6" || stage === "P8") {
  headers.authorization = "Bearer public";
  headers["user-agent"] = COMPOSITE_UA;
}
// P7: refine wire WITHOUT the UA change (reqwest default + auth + tools)
if (stage === "P7") headers.authorization = "Bearer public";
if (stage.startsWith("P4") && !stage.startsWith("P5") && !stage.startsWith("P6")) {
  headers["x-opencode-client"] = "cli";
  headers["x-opencode-request"] = mid;
  headers["x-opencode-project"] = "global";
  headers["user-agent"] = COMPOSITE_UA;
}

// refine-tools schemas() — verbatim shapes (question description shortened:
// prose content is not under test, structure is)
const TOOLS = [
  { type: "function", function: { name: "bash", description: "Run a shell command and return its output.", parameters: { type: "object", properties: { command: { type: "string" }, timeout: { type: "number" } }, required: ["command"] } } },
  { type: "function", function: { name: "read", description: "Read a file from the filesystem.", parameters: { type: "object", properties: { filePath: { type: "string" }, offset: { type: "number" }, limit: { type: "number" } }, required: ["filePath"] } } },
  { type: "function", function: { name: "write", description: "Write content to a file (creates or overwrites).", parameters: { type: "object", properties: { filePath: { type: "string" }, content: { type: "string" } }, required: ["filePath", "content"] } } },
  { type: "function", function: { name: "edit", description: "Replace exact text in a file.", parameters: { type: "object", properties: { filePath: { type: "string" }, oldText: { type: "string" }, newText: { type: "string" } }, required: ["filePath", "oldText", "newText"] } } },
  { type: "function", function: { name: "glob", description: "Find files by glob pattern.", parameters: { type: "object", properties: { pattern: { type: "string" }, path: { type: "string" } }, required: ["pattern"] } } },
  { type: "function", function: { name: "grep", description: "Regex search file contents.", parameters: { type: "object", properties: { pattern: { type: "string" }, path: { type: "string" }, include: { type: "string" } }, required: ["pattern"] } } },
];

const body = { model: "big-pickle", messages: [{ role: "user", content: "Reply with exactly: PROBE_OK" }], stream: true, stream_options: { include_usage: true }, max_tokens: 32000 };
if (stage === "P6" || stage === "P7") {
  // body stays without tools (skip the tools block entirely)
} else if (stage !== "P4a") {
  if (stage === "P4b") {
    body.tools = [];
  } else if (stage === "P4c") {
    body.tools = TOOLS;
    body.tool_choice = "none";
  } else {
    body.tools = TOOLS;
    body.tool_choice = stage === "P8" ? "none" : "auto";
  }
}

(async () => {
  try {
    const resp = await fetch("https://opencode.ai/zen/v1/chat/completions", {
      method: "POST",
      headers,
      body: JSON.stringify(body),
    });
    const text = await resp.text();
    console.log(`[${stage}] ${resp.status} ${text.slice(0, 160)}`);
    console.log(resp.status === 200 ? `${stage}_HIT` : `${stage}_MISS`);
  } catch (e) {
    console.log(`[${stage}] EXC ${e && e.message}`);
  }
})();
