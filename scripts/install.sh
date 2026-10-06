#!/usr/bin/env bash
# refine install — explicit, preferred, NEVER implicit (SRE §5).
#
# The systemd service is an optional overlay: foreground `refine serve` is the
# contract (tests/replay/harnesses rely on it) and gains zero requirements
# from this script. Nothing in the build, tests, or doctor ever invokes this.
#
# Default = full install (build-if-needed → render deploy/refine.service →
# enable → health-probe). Opt-outs:
#   --bin-only   place/report the binary only; do not touch systemd
#   --dry-run    print exactly what would change; touch nothing
#   --bin PATH   binary to run in place (default: target/release/refine)
# Non-systemd hosts fall back to bin-only with a note.
# Never sudo, never touches non-refine units (check-guards rule 6).
set -euo pipefail
cd "$(dirname "$0")/.."
REPO=$PWD

BIN_ONLY=0 DRY=0 SRC=""
while [ $# -gt 0 ]; do
  case "$1" in
    --bin-only) BIN_ONLY=1 ;;
    --dry-run)  DRY=1 ;;
    --bin)      SRC=$2; shift ;;
    -h|--help)  sed -n '2,16p' "$0"; exit 0 ;;
    *) echo "install.sh: unknown flag: $1" >&2; exit 2 ;;
  esac
  shift
done

say() { printf '%s\n' "$*"; }

# --- systemd user session? (read-only probe) --------------------------------
SD=1
command -v systemctl >/dev/null 2>&1 || SD=0
if [ "$SD" = 1 ] && ! systemctl --user show-environment >/dev/null 2>&1; then SD=0; fi
if [ "$SD" = 0 ]; then
  [ "$BIN_ONLY" = 0 ] && say "note: no systemd user session — bin-only install"
  BIN_ONLY=1
fi

# --- resolve binary (used in place; no copying) -----------------------------
SRC=${SRC:-$REPO/target/release/refine}
if [ ! -x "$SRC" ]; then
  if [ "$DRY" = 1 ]; then
    say "[dry-run] cargo build --release   # $SRC missing"
  else
    say "building release binary…"
    cargo build --release
  fi
fi
SRC=$(readlink -f "$SRC" 2>/dev/null || echo "$SRC")

# systemd %-forms for paths under $HOME
case "$SRC" in
  "$HOME"/*) BIN_U="%h${SRC#"$HOME"}" ;;
  *)         BIN_U="$SRC" ;;
esac
case "$REPO" in
  "$HOME"/*) CWD_U="%h${REPO#"$HOME"}" ;;
  *)         CWD_U="$REPO" ;;
esac

say "binary: $SRC"
[ "$BIN_ONLY" = 1 ] && {
  say "bin-only: not touching systemd."
  case "$SRC" in "$HOME"/*) say "hint: add $(dirname "$SRC") to PATH, or run with systemd for restart/resilience layers";; esac
  exit 0
}

# --- render the template (single source of truth) ---------------------------
UNIT_DIR="$HOME/.config/systemd/user"
TEMPLATE="$REPO/deploy/refine.service"
LIVE="$UNIT_DIR/refine.service"
RENDERED=$(mktemp "${TMPDIR:-/tmp}/refine-unit.XXXXXX")
trap 'rm -f "$RENDERED"' EXIT
python3 - "$TEMPLATE" "$RENDERED" "$BIN_U" "$CWD_U" <<'PY'
import re, sys
tpl, out, bin_u, cwd_u = sys.argv[1:5]
s = open(tpl).read()
s, n1 = re.subn(r'(?m)^ExecStart=.*$', f'ExecStart={bin_u} serve --port 4912', s, count=1)
s, n2 = re.subn(r'(?m)^WorkingDirectory=.*$', f'WorkingDirectory={cwd_u}', s, count=1)
assert n1 == 1 and n2 == 1, f"template shape changed (ExecStart={n1}, WorkingDirectory={n2}) — update install.sh"
open(out, "w").write(s)
PY

if [ -f "$LIVE" ]; then
  if diff -q "$LIVE" "$RENDERED" >/dev/null; then
    say "unit: $LIVE (already current)"
  else
    say "unit: $LIVE — diff (live → rendered):"
    diff -u "$LIVE" "$RENDERED" | sed -n '1,40p' || true
  fi
else
  say "unit: $LIVE (new)"
fi

if [ "$DRY" = 1 ]; then
  say "[dry-run] would: write unit, daemon-reload, enable --now refine.service, health-probe :4912"
  say "[dry-run] receipt: ~/.local/share/refine/install-receipt (BIN=$SRC)"
  exit 0
fi

# --- install ----------------------------------------------------------------
mkdir -p "$UNIT_DIR" "$(dirname "$HOME/.local/share/refine/install-receipt")"
printf 'BIN=%s\nINSTALLED=%s\n' "$SRC" "$(date -Is)" > "$HOME/.local/share/refine/install-receipt"
cp "$RENDERED" "$LIVE"
systemctl --user daemon-reload
if systemctl --user is-active refine.service >/dev/null 2>&1; then
  systemctl --user restart refine.service
else
  systemctl --user enable --now refine.service
fi

# --- health probe -----------------------------------------------------------
PORT=4912
PORT=$(grep -oE '\-\-port [0-9]+' "$LIVE" | head -1 | awk '{print $2}')
python3 - "$PORT" <<'PY' || { echo "install: health probe FAILED on :$PORT" >&2; exit 1; }
import sys, time, urllib.request
port = sys.argv[1]
for _ in range(80):
    try:
        r = urllib.request.urlopen(f"http://127.0.0.1:{port}/global/health", timeout=1)
        print(f"health: {r.status} {r.read()[:60].decode(errors='replace')}")
        sys.exit(0)
    except Exception:
        time.sleep(0.25)
sys.exit(1)
PY

say ""
say "installed (service = optional overlay; foreground serve still works untouched):"
say "  unit:     $LIVE"
say "  binary:   $SRC"
say "  data:     ~/.local/share/refine   (sessions live here)"
say "  overrides: systemctl --user edit refine   (MemoryMax, OOMPolicy…)"
say "  kill switches: REFINE_PLUGIN_NORMALIZE=0  REFINE_SIDECAR_RECYCLE_MB=0  REFINE_CGROUP_PARTITION=0"
say "  optional timers (source checkout): systemctl --user enable refine-nightly.timer"
say "  uninstall (keeps history): scripts/uninstall.sh   [--purge for full wipe]"
