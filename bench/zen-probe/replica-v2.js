// FreeTier discriminator bisect — one axis per stage.
//   E1: node  + real headers + tiny body     (headers-only axis)
//   E2: bun   + real headers + tiny body     (+ bun TLS/runtime stack axis)
//   E3: node  + real headers + real 62KB body(+ body-shape axis)
//   E4: bun   + real headers + real 62KB body(bun + full body = near-real)
// Any 200 flips the stage's axis → follow-up narrows the exact field.
// Authorization stays "Bearer public" (captured len=13 — no minted token).
// A5 rate cap: this script makes exactly ONE request per invocation.
const fs = require("fs");
const crypto = require("crypto");

const stage = (process.argv[2] || "E1").toUpperCase();
const USE_BUN_BODY = ["E3","E4","E5","E6","E7","E8","E9"].includes(stage);
// E10 = minimal: good ids + TINY body (body-shape axis with session axis fixed)
// E11 = final repro recipe (good ids + tiny body) for the 3/3 proof.
const USE_BUN = stage === "E2" || stage === "E4";

// opencode id format: <prefix>_ + 12 hex (time part) + 14 base62 (random).
const rand14 = () => {
  const abc = "0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz";
  const b = crypto.randomBytes(14);
  return Array.from(b, (x) => abc[x % abc.length]).join("");
};
const freshId = (p) => p + crypto.randomBytes(6).toString("hex") + rand14();
// E6 replays their captured ids verbatim (existence/format test).
const REPLAY = { sid: "ses_ef4b4e322ffe1WBPQq2Jfnpc6O", mid: "msg_10b4b1f96001y1a2CRAyI2jcMA" };
let sid, mid;
if (stage === "E6") {
  sid = REPLAY.sid;
  mid = REPLAY.mid;
} else if (stage === "E8") {
  // format-correct session + legacy 26-hex msg (isolates session field)
  sid = freshId("ses_");
  mid = "msg_" + crypto.randomBytes(13).toString("hex");
} else if (stage === "E9") {
  // legacy 64-hex session + format-correct msg (isolates session field)
  sid = "ses_" + crypto.randomBytes(32).toString("hex");
  mid = freshId("msg_");
} else {
  sid = freshId("ses_");
  mid = freshId("msg_");
}

const headers = {
  authorization: "Bearer public",
  "content-type": "application/json",
  "user-agent": "opencode/1.18.31 ai-sdk/provider-utils/4.0.23 runtime/bun/1.3.14",
  "x-opencode-client": "cli",
  "x-opencode-project": "global",
  "x-opencode-request": mid,
  "x-opencode-session": sid,
  accept: "*/*",
};

const tinyBody = JSON.stringify({
  model: "big-pickle",
  max_tokens: 8,
  messages: [{ role: "user", content: "say ok" }],
});
// E12: tiny body + stream flags only (which body element is gated?)
const streamBody = JSON.stringify({
  model: "big-pickle",
  max_tokens: 8,
  stream: true,
  stream_options: { include_usage: true },
  messages: [{ role: "user", content: "say ok" }],
});
// E13: tiny + real tools array (tool-list fingerprint axis)
const toolsBody = (() => {
  const real = JSON.parse(fs.readFileSync("/probe/runs/real-body.json", "utf8"));
  return JSON.stringify({
    model: "big-pickle",
    max_tokens: 8,
    stream: true,
    stream_options: { include_usage: true },
    messages: [{ role: "user", content: "say ok" }],
    tool_choice: "auto",
    tools: real.tools,
  });
})();

let body;
if (USE_BUN_BODY) body = fs.readFileSync("/probe/runs/real-body.json");
else if (stage === "E12") body = Buffer.from(streamBody, "utf8");
else if (stage === "E13") body = Buffer.from(toolsBody, "utf8");
else body = Buffer.from(tinyBody, "utf8");

(async () => {
  try {
    const resp = await fetch("https://opencode.ai/zen/v1/chat/completions", {
      method: "POST",
      headers,
      body,
    });
    const text = await resp.text();
    const runtime = USE_BUN ? "bun" : "node";
    console.log(`[${stage}/${runtime}] ${resp.status} ${text.slice(0, 180)}`);
    if (resp.status === 200) console.log(`${stage}_HIT`);
    else console.log(`${stage}_MISS`);
  } catch (e) {
    console.log(`[${stage}] EXC ${e && e.message}`);
  }
})();
