#!/bin/bash
# In-container scenario runner for zen-probe. Everything (opencode ELF,
# replica, capture, proxy) runs here — host services are never touched
# (AGENTS §2 rule 11). Args arrive via `docker run zen-probe:latest <scenario>`.
set -u
OC=/opt/oc/opencode
PORT=4909
RUNS=/probe/runs
mkdir -p "$RUNS"

stage_config() {
  mkdir -p "$HOME/.config/opencode" "$HOME/.local/share/opencode" \
           "$HOME/.local/state/opencode" "$HOME/.cache/opencode"
  cp /mnt/cfg/opencode.json  "$HOME/.config/opencode/opencode.json"
  cp /mnt/cfg/auth.json      "$HOME/.local/share/opencode/auth.json"
  cp /mnt/cfg/models.json    "$HOME/.cache/opencode/models.json"
  [ -f /mnt/cfg/model.json ]    && cp /mnt/cfg/model.json    "$HOME/.local/state/opencode/model.json"
  [ -f /mnt/cfg/account.json ]  && cp /mnt/cfg/account.json  "$HOME/.local/share/opencode/account.json"
  return 0
}

boot() {
  "$OC" serve --port "$PORT" --hostname 127.0.0.1 >>"$RUNS/oc-serve.log" 2>&1 &
  OCPID=$!
  local i
  for i in $(seq 1 60); do
    curl -sf --max-time 3 "http://127.0.0.1:$PORT/global/health" >/dev/null 2>&1 && return 0
    sleep 0.5
  done
  echo "BOOT_FAIL — serve log tail:"; tail -5 "$RUNS/oc-serve.log"
  return 1
}

stop() {
  if [ -n "${OCPID:-}" ]; then kill "$OCPID" 2>/dev/null; wait "$OCPID" 2>/dev/null; OCPID=""; fi
}
cap_start() {
  tcpdump -i eth0 -s 0 -w "$RUNS/$1.pcap" >/dev/null 2>&1 &
  TCPID=$!; sleep 0.5
}
cap_stop() {
  if [ -n "${TCPID:-}" ]; then kill "$TCPID" 2>/dev/null; wait "$TCPID" 2>/dev/null; TCPID=""; fi
}
cleanup() { cap_stop; stop; }
trap cleanup EXIT

# Poll the session until an assistant message has time.completed (freeze shape),
# then print DONE or WAIT; on DONE print the assistant text tail.
poll_done() {
  curl -sf --max-time 5 "http://127.0.0.1:$PORT/session/$1/message" | node -e '
    let d=""; process.stdin.on("data",c=>d+=c).on("end",()=>{
      try{
        const a=JSON.parse(d);
        if(!Array.isArray(a)) return console.log("WAIT");
        const asst=a.filter(m=>m&&m.info&&m.info.role==="assistant");
        const done=asst.some(m=>m.info.time&&m.info.time.completed);
        if(!done) return console.log("WAIT");
        const last=asst[asst.length-1];
        const texts=(last.parts||[]).filter(p=>p.type==="text").map(p=>p.text).join(" ");
        console.log("DONE "+JSON.stringify(texts.slice(0,200)));
      }catch(e){ console.log("WAIT"); }
    });'
}

s0() {
  # G1 gate: the real client (their exact ELF, staged config) completes big-pickle.
  boot || return 1
  cap_start s0
  local sid
  echo "creating-session..."
  curl -s --max-time 60 -o "$RUNS/s0-create.body" -w 'create_http=%{http_code} time=%{time_total}s\n' -X POST "http://127.0.0.1:$PORT/session" -H 'content-type: application/json' -d '{"title":"z0"}'
  echo "create_body=$(head -c 300 "$RUNS/s0-create.body")"
  sid=$(node -e 'let d="";try{d=require("fs").readFileSync(process.argv[1],"utf8");console.log(JSON.parse(d).id||"")}catch(e){console.log("")}' "$RUNS/s0-create.body")
  echo "session=${sid:-NONE}"
  if [ -z "$sid" ]; then echo "S0_FAIL no-session"; cap_stop; return 1; fi
  local code
  code=$(curl -s --max-time 15 -o "$RUNS/s0-prompt.body" -w '%{http_code}' -X POST \
        "http://127.0.0.1:$PORT/session/$sid/prompt_async" -H 'content-type: application/json' \
        -d '{"model":{"providerID":"opencode","modelID":"big-pickle"},"parts":[{"type":"text","text":"Reply with exactly: PROBE_OK"}]}')
  echo "prompt_async=$code body=$(head -c 200 "$RUNS/s0-prompt.body")"
  local i out=""
  for i in $(seq 1 45); do
    out=$(poll_done "$sid")
    case "$out" in DONE*) break ;; esac
    sleep 2
  done
  echo "$out"
  case "$out" in
    DONE*PROBE_OK*) echo "S0_PASS" ;;
    DONE*)          echo "S0_COMPLETED_TEXT_MISMATCH" ;;
    *)              echo "S0_FAIL no-completion-in-90s"
                    echo "-- messages --"
                    curl -s "http://127.0.0.1:$PORT/session/$sid/message" | head -c 1200; echo
                    echo "-- serve log tail --"; tail -20 "$RUNS/oc-serve.log" ;;
  esac
  cap_stop
}

s1() {
  # Control: our replica must reproduce 403 in the same container/network.
  cap_start s1
  node /probe/replica-node.js | tee "$RUNS/s1.out"
  cap_stop
  if grep -q '403' "$RUNS/s1.out"; then echo "S1_CONTROL_403_OK"; else echo "S1_UNEXPECTED"; fi
}

r1() {
  # Logging CONNECT proxy: does the ELF honor https_proxy for its LLM path?
  node /probe/proxy-log.js >"$RUNS/r1-proxy.log" 2>&1 &
  PXY=$!
  sleep 0.5
  export HTTPS_PROXY=http://127.0.0.1:8888 HTTP_PROXY=http://127.0.0.1:8888 \
         https_proxy=http://127.0.0.1:8888 http_proxy=http://127.0.0.1:8888 \
         NO_PROXY=127.0.0.1,localhost,::1 no_proxy=127.0.0.1,localhost,::1
  s0
  local rc=$?
  unset HTTPS_PROXY HTTP_PROXY https_proxy http_proxy NO_PROXY no_proxy
  sleep 1
  kill "$PXY" 2>/dev/null || true
  echo "== proxy log =="
  cat "$RUNS/r1-proxy.log"
  local n
  n=$(grep -c '^CONNECT ' "$RUNS/r1-proxy.log" || true)
  echo "R1_CONNECT_LINES=$n"
  if [ "$n" -gt 0 ]; then echo "R1_PROXY_HONORED"; else echo "R1_PROXY_IGNORED_OR_ONLY_DIRECT"; fi
  return $rc
}

s1b() {
  # Replica under bun (their runtime): does the stack alone flip 403 -> 200?
  cap_start s1b
  /opt/bun run /probe/replica-node.js | tee "$RUNS/s1b.out"
  cap_stop
  if grep -q '403' "$RUNS/s1b.out"; then echo "S1B_STILL_403"; else echo "S1B_NOT_403_CHECK_OUTPUT"; fi
}

r3() {
  # JA3/ALPN of the working flow (s0.pcap) vs the 403 replica (s1.pcap).
  for s in s0 s1; do
    echo "== $s =="
    tshark -r "$RUNS/$s.pcap" \
      -Y 'tls.handshake.type==1 && tls.handshake.extensions_server_name==opencode.ai' \
      -T fields -e tcp.srcport -e tls.handshake.ja3 -e tls.handshake.extensions_alpn_str \
      2>/dev/null || echo "tshark-extract-failed"
  done
}

stage_config
case "${1:-}" in
  s0)  s0 ;;
  s1)  s1 ;;
  s1b) s1b ;;
  r1)  r1 ;;
  r3)  r3 ;;
  rv2)
    shift
    STAGE="${1:-E1}"
    cap_start "rv2-$STAGE"
    case "$STAGE" in
      E2|E4|E6|E7|E8|E9|E10|E11|E12|E13) /opt/bun run /probe/replica-v2.js "$@" ;;
      *)     node /probe/replica-v2.js "$@" ;;
    esac
    cap_stop
    ;;
  boot-check) boot && echo BOOT_OK ;;
  *) echo "usage: s0|s1|s1b|r1|r3|rv2 E1..E4|boot-check" >&2; exit 2 ;;
esac
