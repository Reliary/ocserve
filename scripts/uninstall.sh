#!/usr/bin/env bash
# refine uninstall — clean, idempotent, and it NEVER touches shared opencode
# state (config, auth, legacy db, model cache, plugin packages — the
# 2026-10-05 blast-radius class; check-guards rule 11 guards this file).
#
#   (default)      remove the APPLICATION: refine* units/timers/drop-ins,
#                  the binary, refine-derived .normalized.mjs artifacts.
#                  KEEP session history — printed with sizes + purge hint.
#   --purge        also remove data/state: stats → confirm → optional
#                  VACUUM INTO backup → delete.  (--yes for non-interactive;
#                  --no-backup to skip the backup)
#   --dry-run      full inventory, touches nothing
#   --selftest     staged fake-HOME canary battery (refine gone, canaries
#                  byte-identical, idempotent) — no systemd involvement
#   REFINE_UNINSTALL_SYSTEMD=0   file operations only (used by --selftest)
#
# Deliberately NOT done (documented in SRE §5): journal vacuum — the journal
# is shared with opencode and unit logs age out via the retention floor.
set -euo pipefail

PURGE=0 DRY=0 YES=0 BACKUP=1 SELFTEST=0
while [ $# -gt 0 ]; do
  case "$1" in
    --purge)    PURGE=1 ;;
    --dry-run)  DRY=1 ;;
    --yes)      YES=1 ;;
    --no-backup) BACKUP=0 ;;
    --selftest) SELFTEST=1 ;;
    -h|--help)  sed -n '2,20p' "$0"; exit 0 ;;
    *) echo "uninstall.sh: unknown flag: $1" >&2; exit 2 ;;
  esac
  shift
done

say() { printf '%s\n' "$*"; }
SELF_BIN=$(readlink -f "$0")

# Refuse anything that is not under $HOME: belt & braces against a hostile
# or mangled Environment= line ever steering a delete outside the home tree.
require_home() {
  case "$1" in
    "$HOME"/*) ;;
    *) echo "REFUSING path outside HOME: $1" >&2; exit 3 ;;
  esac
}
# Data/state removal additionally demands "refine" in the path: a custom
# REFINE_DATA_DIR without it is reported for manual removal, never deleted.
require_refine_path() {
  require_home "$1"
  case "$1" in
    *refine*) ;;
    *) say "NOT removing $1 (path lacks 'refine' — remove manually if desired)"; return 1 ;;
  esac
}

snapshot_unit_env() { # $1=unit file → prints ExecStart / REFINE_DATA_DIR, %h expanded
  [ -f "$1" ] || return 0
  local line
  line=$(grep -m1 '^ExecStart=' "$1" || true)
  [ -n "$line" ] && printf 'BIN=%s\n' "${line#ExecStart=}" | awk '{print $1}' | sed "s|%h|$HOME|"
  line=$(grep -m1 '^Environment=REFINE_DATA_DIR=' "$1" || true)
  [ -n "$line" ] && printf '%s\n' "${line#Environment=}" | sed "s|%h|$HOME|"
}

unit_data_dir() { # best-effort REFINE_DATA_DIR from the installed unit
  local f="$HOME/.config/systemd/user/refine.service"
  if [ -f "$f" ]; then
    grep -m1 '^Environment=REFINE_DATA_DIR=' "$f" 2>/dev/null \
      | sed "s|^Environment=REFINE_DATA_DIR=||; s|%h|$HOME|" || true
  fi
}

derived_files() { # refine-created artifacts inside shared dirs (basename-gated)
  find "$HOME/.cache/opencode/packages" -type f \
    \( -name '*.normalized.mjs' -o -name '*.normalized.mjs.hash' \) 2>/dev/null || true
  python3 - <<'PY'
import json, os
home = os.path.expanduser("~")
cfg = os.path.join(home, ".config", "opencode", "opencode.json")
try:
    conf = json.load(open(cfg))
except Exception:
    raise SystemExit
for e in conf.get("plugin") or []:
    if isinstance(e, str) and os.path.isabs(e):
        stem, _ = os.path.splitext(e)
        for f in (stem + ".normalized.mjs", stem + ".normalized.mjs.hash"):
            if os.path.isfile(f):
                print(f)
PY
}

remove_derived() {
  local f
  while IFS= read -r f; do
    [ -n "$f" ] || continue
    case "$f" in
      *.normalized.mjs|*.normalized.mjs.hash)
        require_home "$f"
        rm -f "$f"
        say "removed derived: $f"
        ;;
      *) echo "REFUSING non-derived path: $f" >&2; exit 3 ;;
    esac
  done
}

db_stats() { # human-readable history warning (also used pre-confirm)
  local db="$1"
  python3 - "$db" <<'PY' || true
import os, sqlite3, sys
db = sys.argv[1]
if not os.path.exists(db):
    print("  history: none")
    raise SystemExit
mb = os.path.getsize(db) / 1048576
try:
    c = sqlite3.connect(f"file:{db}?mode=ro", uri=True)
    n = c.execute("select count(*) from session").fetchone()[0]
    print(f"  history: {n} sessions, {mb:.0f} MB ({db})")
except Exception as e:
    print(f"  history: {mb:.0f} MB (db unreadable: {e})")
PY
}

# Orphan sweep — only real (systemd) uninstalls, anchored to THIS install's
# data dir so staged tests can never match a live sidecar; never kills self/parent.
sweep_orphans() {
  local data=$1 p
  for p in $(pgrep -f -- "$data/plugin-host/host.mjs" 2>/dev/null; \
             pgrep -f -- '/tmp/refine-repl' 2>/dev/null); do
    [ "$p" = "$$" ] && continue
    [ "$p" = "$PPID" ] && continue
    kill "$p" 2>/dev/null && say "swept orphan plugin host pid $p" || true
  done
}

# --- staged canary battery ---------------------------------------------------
run_selftest() {
  local fake rc=0
  fake=$(mktemp -d "${TMPDIR:-/tmp}/refine-uninstall-st.XXXXXX")
  mkdir -p "$fake/.config/systemd/user/refine.service.d" \
           "$fake/.local/share/refine/plugin-host" \
           "$fake/.local/state/refine/soak" \
           "$fake/.local/bin" \
           "$fake/.cache/opencode/packages/@x/pkg/node_modules/@x/pkg/dist" \
           "$fake/.config/opencode" \
           "$fake/.cache/opencode" \
           "$fake/.local/share/opencode" \
           "$fake/src/reliary8/opencode-plugin/dist" \
           "$fake/src/refine/target/release"
  # refine artifacts
  printf 'ExecStart=%%h/src/refine/target/release/refine serve --port 4912\nEnvironment=REFINE_DATA_DIR=%%h/.local/share/refine\n' \
    > "$fake/.config/systemd/user/refine.service"
  printf 'timer' > "$fake/.config/systemd/user/refine-nightly.timer"
  printf 'drop'  > "$fake/.config/systemd/user/refine.service.d/extra.conf"
  printf 'BIN=%s\n' "$fake/.local/bin/refine" > "$fake/.local/share/refine/install-receipt"
  printf 'ELF-binary' > "$fake/.local/bin/refine"
  printf 'ELF-dev'    > "$fake/src/refine/target/release/refine"
  python3 - "$fake/.local/share/refine/refine.db" <<'PY'
import sqlite3, sys
c = sqlite3.connect(sys.argv[1])
c.execute("create table session (id text)")
c.execute("insert into session values ('s1')")
c.commit(); c.close()
PY
  # canaries (shared opencode state + non-refine units)
  printf 'config-canary'   > "$fake/.config/opencode/opencode.json"
  printf 'auth-canary'     > "$fake/.local/share/opencode/auth.json"
  printf 'models-canary'   > "$fake/.cache/opencode/models.json"
  printf 'oc-unit-canary'  > "$fake/.config/systemd/user/opencode.service"
  printf 'pkg-keep'        > "$fake/.cache/opencode/packages/@x/pkg/node_modules/@x/pkg/dist/index.js"
  # derived files (one in packages cache, one beside an absolute-path plugin)
  printf 'norm'  > "$fake/.cache/opencode/packages/@x/pkg/node_modules/@x/pkg/dist/index.normalized.mjs"
  printf 'hash'  > "$fake/.cache/opencode/packages/@x/pkg/node_modules/@x/pkg/dist/index.normalized.mjs.hash"
  printf 'norm'  > "$fake/src/reliary8/opencode-plugin/dist/index.normalized.mjs"
  python3 - "$fake" <<'PY'
import json, os, sys
fake = sys.argv[1]
json.dump({"plugin": [os.path.join(fake, "src/reliary8/opencode-plugin/dist/index.js")]},
          open(os.path.join(fake, ".config/opencode/opencode.json"), "w"))
PY
  printf '{"plugin": ["%s/src/reliary8/opencode-plugin/dist/index.js"]}' "$fake" \
    > "$fake/.config/opencode/opencode.json"

  say "== selftest: default run (history must survive) =="
  if ! HOME="$fake" REFINE_UNINSTALL_SYSTEMD=0 "$SELF_BIN"; then echo "FAIL: default run rc!=0"; rc=1; fi
  check() { # must-exist $1, must-be-gone $2, label $3
    if [ ! -e "$1" ]; then echo "FAIL($3): missing $1"; rc=1; fi
    if [ -e "$2" ]; then echo "FAIL($3): should be gone: $2"; rc=1; fi
  }
  # units/drop-ins/binary(receipt)/derived gone
  check "$fake/.local/share/refine/refine.db" \
        "$fake/.config/systemd/user/refine.service" "units"
  [ -e "$fake/.config/systemd/user/refine-nightly.timer" ] && { echo "FAIL: timer remains"; rc=1; }
  [ -e "$fake/.config/systemd/user/refine.service.d" ] && { echo "FAIL: drop-in dir remains"; rc=1; }
  [ -e "$fake/.local/bin/refine" ] && { echo "FAIL: receipt binary remains"; rc=1; }
  [ -e "$fake/src/refine/target/release/refine" ] || { echo "FAIL: dev-build binary must be KEPT"; rc=1; }
  [ -e "$fake/.cache/opencode/packages/@x/pkg/node_modules/@x/pkg/dist/index.normalized.mjs" ] && { echo "FAIL: derived remains"; rc=1; }
  [ -e "$fake/src/reliary8/opencode-plugin/dist/index.normalized.mjs" ] && { echo "FAIL: path-plugin derived remains"; rc=1; }
  # canaries byte-identical
  for f in "$fake/.config/opencode/opencode.json" "$fake/.local/share/opencode/auth.json" \
           "$fake/.cache/opencode/models.json" "$fake/.config/systemd/user/opencode.service" \
           "$fake/.cache/opencode/packages/@x/pkg/node_modules/@x/pkg/dist/index.js"; do
    case "$f" in
      *opencode.json) grep -q 'reliary8' "$f" || { echo "FAIL: config canary mutated: $f"; rc=1; } ;;
      *) [ "$(cat "$f")" = "$(case "$f" in *auth.json) echo auth-canary;; *models.json) echo models-canary;; *opencode.service) echo oc-unit-canary;; *index.js) echo pkg-keep;; esac)" ] \
           || { echo "FAIL: canary mutated: $f"; rc=1; } ;;
    esac
  done
  # history kept (default)
  [ -e "$fake/.local/share/refine/refine.db" ] || { echo "FAIL: default run must KEEP history"; rc=1; }
  [ -e "$fake/.local/state/refine/soak" ] || { echo "FAIL: default run must KEEP state"; rc=1; }

  say "== selftest: idempotent second run =="
  HOME="$fake" REFINE_UNINSTALL_SYSTEMD=0 "$SELF_BIN" || { echo "FAIL: second run rc!=0"; rc=1; }

  say "== selftest: purge --yes (history goes, canaries stay) =="
  HOME="$fake" REFINE_UNINSTALL_SYSTEMD=0 "$SELF_BIN" --purge --yes || { echo "FAIL: purge rc!=0"; rc=1; }
  [ -e "$fake/.local/share/refine" ] && { echo "FAIL: purge left data dir"; rc=1; }
  [ -e "$fake/.local/state/refine" ] && { echo "FAIL: purge left state dir"; rc=1; }
  [ "$(cat "$fake/.local/share/opencode/auth.json")" = "auth-canary" ] || { echo "FAIL: purge ate auth"; rc=1; }
  grep -q 'reliary8' "$fake/.config/opencode/opencode.json" || { echo "FAIL: purge ate config"; rc=1; }
  [ "$(cat "$fake/.config/systemd/user/opencode.service")" = "oc-unit-canary" ] || { echo "FAIL: purge ate opencode unit"; rc=1; }

  rm -rf "$fake"
  if [ "$rc" = 0 ]; then echo "selftest OK (canaries intact, idempotent, history honored)"; exit 0
  else echo "selftest FAILED"; exit 1; fi
}

[ "$SELFTEST" = 1 ] && run_selftest

# --- inventory (also the dry-run report) -------------------------------------
HOME_DIR=${HOME:?HOME unset}
UNIT_DIR="$HOME_DIR/.config/systemd/user"
shopt -s nullglob
UNITS=("$UNIT_DIR"/refine*.service "$UNIT_DIR"/refine*.timer)
DROPINS=("$UNIT_DIR"/refine*.service.d)
shopt -u nullglob
DATA_DIR=$(unit_data_dir)
DATA_DIR=${DATA_DIR:-${REFINE_DATA_DIR:-$HOME_DIR/.local/share/refine}}
DATA_DIR=${DATA_DIR/#%h/$HOME_DIR}
STATE_DIR=${XDG_STATE_HOME:-$HOME_DIR/.local/state}/refine
RECEIPT_BIN=""
[ -f "$DATA_DIR/install-receipt" ] && RECEIPT_BIN=$(grep -m1 '^BIN=' "$DATA_DIR/install-receipt" | cut -d= -f2- || true)
UNIT_BIN=""
[ -f "$UNIT_DIR/refine.service" ] && UNIT_BIN=$(snapshot_unit_env "$UNIT_DIR/refine.service" | sed -n 's/^BIN=//p' | head -1)
BIN=${RECEIPT_BIN:-$UNIT_BIN}

SD=1
command -v systemctl >/dev/null 2>&1 || SD=0
if [ "$SD" = 1 ] && ! systemctl --user show-environment >/dev/null 2>&1; then SD=0; fi
if [ "${REFINE_UNINSTALL_SYSTEMD:-1}" = 0 ]; then SD=0; fi

if [ "$DRY" = 1 ]; then
  say "== dry-run inventory (nothing will be touched) =="
  say "units/timers/drop-ins:"
  [ ${#UNITS[@]} -eq 0 ] && say "  (none)"
  for u in "${UNITS[@]}"; do say "  $u"; done
  for d in "${DROPINS[@]}"; do say "  $d/"; done
  say "binary: ${BIN:-'(not attributable — remove manually if desired)'}"
  [ -n "$BIN" ] && [ -e "$BIN" ] && say "  exists: yes ($(stat -c%s "$BIN") B)" || true
  case "$BIN" in */target/*) say "  dev build (kept on real uninstall)";; esac
  say "data (kept unless --purge):"
  [ -d "$DATA_DIR" ] && du -sh "$DATA_DIR" 2>/dev/null | sed 's/^/  /' || say "  (none)"
  db_stats "$DATA_DIR/refine.db"
  [ -d "$STATE_DIR" ] && du -sh "$STATE_DIR" 2>/dev/null | sed 's/^/  /' || true
  say "derived artifacts to remove (always):"
  derived_files | sed 's/^/  /'
  say "shared opencode state that STAYS: ~/.config/opencode, auth.json, opencode.db, models.json, plugin packages"
  say "unit logs STAY (shared journal ages via retention floor; vacuum would hit opencode logs too)"
  exit 0
fi

# --- purge confirm (before any destructive step) -----------------------------
if [ "$PURGE" = 1 ] && [ "$YES" != 1 ]; then
  say "--purge will DESTROY session history:"
  db_stats "$DATA_DIR/refine.db"
  if [ -t 0 ]; then
    printf 'remove history too? [y/N] '
    read -r reply
    case "$reply" in y|Y|yes|YES) ;; *) say "aborted (nothing removed)"; exit 0 ;; esac
  else
    echo "non-interactive without --yes: refusing to purge (default install keeps history)" >&2
    exit 2
  fi
fi

# --- stop (real systemd only; refuse if it will not stop) --------------------
if [ "$SD" = 1 ]; then
  for u in "${UNITS[@]}"; do
    bn=$(basename "$u")
    case "$bn" in
      *.service.d) continue ;;
      *) systemctl --user disable --now "$bn" >/dev/null 2>&1 || true ;;
    esac
  done
  systemctl --user reset-failed >/dev/null 2>&1 || true
  if systemctl --user is-active refine.service >/dev/null 2>&1; then
    echo "refuse: refine.service still active after stop — investigate before uninstall" >&2
    exit 1
  fi
  sweep_orphans "$DATA_DIR"
fi

# --- remove unit files --------------------------------------------------------
for u in "${UNITS[@]}"; do
  require_home "$u"
  case "$u" in *refine*) rm -f "$u"; say "removed unit: $u" ;; esac
done
for d in "${DROPINS[@]}"; do
  require_home "$d"
  case "$d" in *refine*) rm -rf "$d"; say "removed drop-ins: $d" ;; esac
done
[ "$SD" = 1 ] && systemctl --user daemon-reload

# --- derived artifacts (basename-gated; shared dirs lose only OUR files) ------
remove_derived < <(derived_files)

# --- binary -------------------------------------------------------------------
if [ -n "$BIN" ]; then
  case "$BIN" in
    */target/*)
      say "keeping dev-build binary: $BIN   (source tree — cargo clean reclaims it)" ;;
    "$HOME_DIR"/*)
      if [ -e "$BIN" ]; then require_home "$BIN"; rm -f "$BIN"; say "removed binary: $BIN"; fi ;;
    *)
      say "keeping binary outside HOME: $BIN   (remove manually if desired)" ;;
  esac
else
  say "binary: not attributable (no receipt/unit) — remove manually if desired"
fi

# --- history ------------------------------------------------------------------
if [ "$PURGE" = 1 ]; then
  if [ "$BACKUP" = 1 ] && [ -f "$DATA_DIR/refine.db" ]; then
    DEST="$HOME_DIR/refine-backup-$(date +%Y%m%d-%H%M%S).db"
    python3 - "$DATA_DIR/refine.db" "$DEST" <<'PY' || { echo "backup failed — aborting purge" >&2; exit 1; }
import sqlite3, sys
src, dest = sys.argv[1], sys.argv[2]
c = sqlite3.connect(src)
c.execute("VACUUM INTO ?", (dest,))
c.close()
print(f"backup: {dest}")
PY
  fi
  for target in "$DATA_DIR" "$STATE_DIR"; do
    if require_refine_path "$target"; then
      rm -rf "$target"
      say "removed: $target"
    fi
  done
else
  say ""
  say "history KEPT: $DATA_DIR"
  db_stats "$DATA_DIR/refine.db"
  say "  full wipe later: scripts/uninstall.sh --purge"
fi

say "uninstall complete."
