// FreeTier gate discriminator test: same headers as upstream would send,
// but through Node's fetch (undici) = the app's own HTTP+TLS stack.
const headers = {
  "content-type": "application/json",
  authorization: "Bearer public",
  "user-agent": "opencode/1.18.31",
  "x-opencode-session": "ses_ef6907c85ffe6TokiMy6mi1FG3",
  "x-opencode-session-id": "ses_ef6907c85ffe6TokiMy6mi1FG3",
  "x-opencode-client": "tui",
  "x-opencode-project": "global",
  "x-opencode-request": "msg_zenprobe0000000000000002",
};

async function probe(tag, extra = {}) {
  const body = JSON.stringify({
    model: "big-pickle",
    max_tokens: 8,
    messages: [{ role: "user", content: "say ok" }],
  });
  try {
    const resp = await fetch("https://opencode.ai/zen/v1/chat/completions", {
      method: "POST",
      headers: { ...headers, ...extra },
      body,
    });
    const text = await resp.text();
    console.log(`[${tag}] ${resp.status} ${text.slice(0, 160)}`);
  } catch (e) {
    console.log(`[${tag}] EXC ${e && e.message}`);
  }
}

(async () => {
  await probe("node-full-set");
})();
