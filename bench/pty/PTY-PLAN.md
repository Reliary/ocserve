# PTY + compat batch — live crash fix and SDK-surface closure

Status: **building** (2026-10-08). Driver: the user hit
`TypeError: e.shells.reduce is not a function` in the web UI settings page;
root cause proven: `GET /pty/shells` is not implemented, our catch-all
proxies the path to app.opencode.ai, the UI receives HTML where it expects
`Shell[]`, and crashes. The same class applies to **every SDK route we
don't implement** — the proxy fallback turns a missing JSON route into an
HTML landmine.

## Evidence (freeze 1.18.31, probed live 2026-10-08)

- `GET /pty/shells` → 200 `[{path,name,acceptable}...]` from `/etc/shells`
  (fish `acceptable:false`; dedup by path, order preserved).
- `GET /pty` → list of Info (running+exited until removed).
- `POST /pty` `{command?,args?,cwd?,title?,env?}` → Info
  `{id,title,command,args,cwd,status,pid}`; id `pty_`+26 ascending;
  title default `Terminal <last4>`; args get `-l` appended for login shells
  bash/sh/zsh/…; cwd defaults to server working directory (freeze used its
  cwd `~/src/ocserve`); env `TERM=xterm-256color`,
  `OPENCODE_TERMINAL=1`.
- `GET /pty/{id}` → Info; unknown → 404 `{"_tag":"PtyNotFoundError","ptyID",
  "message":"PTY session not found: <id>"}`; malformed (not `pty_`) → 400
  `{"name":"BadRequest","data":{"message":"Expected a string starting with
  \"pty\", got \"x\"\n  at [\"ptyID\"]","kind":"Params"}}`
- `PUT /pty/{id}` (NOT PATCH; PATCH falls through to the SPA) `{title?,
  size?}` → Info. `DELETE /pty/{id}` → `true` (JSON), unknown → 404 tag.
- `POST /pty/{id}/connect-token` requires header `x-opencode-ticket: 1` and
  same-origin/frozen-CORS Origin → `{ticket, expires_in:60}`; wrong origin
  or missing header → 403 `{"_tag":"PtyForbiddenError","message":"Invalid
  PTY connect token request"}`; unknown id (valid origin) → 404 tag.
  Ticket = UUID, single-use, 60 s TTL, scoped (ptyID, directory).
- `GET /pty/{id}/connect` (websocket) `?directory&cursor&ticket`.
  Protocol (`core/src/pty/protocol.ts`): replay chunks 64 KiB → one meta
  frame `[0x00, JSON{"cursor":N}]` → live; inbound text/binary → terminal in;
  on exit/removal/404/exit → close frame code 4404 (not found/exited) or
  1000. No ticket + disallowed origin → 403; no ticket + allowed origin →
  allowed (probed: localhost origin 200).
- Events: `pty.created` `{info}`, `pty.updated` `{info}`, `pty.exited`
  `{id,exitCode}`, `pty.deleted` `{id}` — plain frames (no sync twin).
- Buffer: 2 MiB retained; cursor semantics: absolute output chars; -1 tails
  from current end; omitted replays all retained. Exited sessions retained
  (cap 25) — v1 legacy surface hides exited in GET/list/attach.

Additional shapes probed for the batch:
- `GET /session/{id}/message/{mid}` unknown → 404
  `{"name":"NotFoundError","data":{"message":"Message not found: <mid>"}}`
- `POST /session/{id}/permissions/{pid}` `{response}` → 404
  `{"_tag":"PermissionNotFoundError","requestID","message"}` (note:
  requestID key, not permissionID).
- `/session/{id}/diff` is out (REST diff needs a snapshot engine) —
  already `divergence:K-ADMIN`; SPA fallback is the same behavior as any
  unknown path; UI handles it (session view tolerates).
- `/log` `POST {service,level,message,extra?}` → `true`.

## Implementation plan

1. **`ocserve-pty`** (new crate): PtyManager on blocking threads.
   - Session: id, title, command, args, cwd, status, pid, exit_code,
     buffer (2 MiB cap + buffer_cursor), cursor, subscribers.
   - Unix impl in `pty_unix.rs`: `openpty` + `posix_openpt`/`grantpt`/
     `unlockpt` + `fork` + `login_tty` + `execvp` via `libc` (same pattern
     as the frozen binary's node-pty). Reader thread (O_NONBLOCK 10 ms
     poll → broadcast channel), waiter thread (`waitpid` → exit event).
     Resize via `TIOCSWINSZ`. Kill via `killpg(SIGTERM)` then `SIGKILL`.
   - Windows: no-op stubs returning unsupported (documented divergence;
     upstream uses conpty).
   - Retention: exited sessions capped at 25 (FIFO), then removed.
   - IDs: `pty_` + 26 ascending-style (reuse `ocserve_core::ids` scheme).
   - Bus: publish `pty.created/updated/exited/deleted` via a callback the
     HTTP layer injects (crate stays free of HTTP deps), directory from
     `PtyManager.cwd_root`.
2. **Routes** in `ocserve-http` (module `pty.rs`):
   - `GET /pty/shells` (no PTY process; reads `/etc/shells`, dedup, meta
     deny-list for fish/nu).
   - `GET /pty`, `POST /pty`, `GET|PUT|DELETE /pty/{id}`.
   - `POST /pty/{id}/connect-token` with header+origin gates.
   - `GET /pty/{id}/connect` websocket (axum `ws` feature): replay → meta →
     live; inbound → write; close 4404/1000.
3. **Compat batch** (same PR, all "one-shot decisive"):
   - `GET /session/{id}/message/{mid}` (404 envelope as probed).
   - `POST /session/{id}/permissions/{pid}` (legacy; maps to the gate).
   - `PUT /auth/{id}` (body `{type:"api"|"oauth", key?}` → auth.json).
   - `POST /log` → `true`.
   - `POST /session/{id}/init` → runs the built-in `init` command
     (`{messageID,providerID,modelID}` payload).
4. **UI-proxy 404 semantics**: requests with `Accept: application/json` (or
   `/api/*` prefix? no — upstream proxies everything; keep proxying) still
   proxy, but the crash class is fixed by implementing the routes above.
   No behavioral change to the fallback (upstream parity).
5. **Port config**: `serve --port` stays; precedence becomes
   `--port` (explicit) > `OCSERVE_PORT` env > config file > default 4096.
   (CLI arg becomes the override; config support lands with `ocserve.toml`
   in the same batch — minimal: `port`, `hostname` keys via a tiny TOML
   reader.)

## Tests

- Unit: shell-list parsing (dedup, deny-list), ticket store (issue/consume/
  TTL/capacity/scope), buffer ring (cap + cursor math), args `-l` rule.
- Integration (unix): create→info shape; echo round-trip through a real
  pty; resize; exit event + retention; delete → `true`; unknown-envelope
  bytes; connect-token 403/404/200; websocket replay+meta+live+close-4404
  using a tokio-tungstenite client.
- Negative controls: (a) disable the shell-lookup guard → duplicate paths
  test red; (b) bypass the ticket consume → replay-attack test red;
  (c) buffer cap disabled → 2 MiB test red.

## Divergences (recorded)

- Windows PTY unsupported (stub 500) — upstream conpty.
- `workspace` query ignored (single-directory server; freeze behavior on
  this box identical).
- `/session/{id}/diff` remains unimplemented (K-ADMIN).

## Gates

fmt, clippy -D, tests, guards (new rules: PTY buffer bound; ticket TTL),
matrix, replay 26/0; size ceiling re-baselined from measurement.
