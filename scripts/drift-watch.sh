#!/usr/bin/env bash
# Upstream drift watch, v1 line (PLAN §6 nightly detection, PLAN §8 adoption
# posture). READ-ONLY TRIAGE — never auto-adopts.
#
# History (2026-11-03): this script replayed the recorded corpus between the
# control arm (freeze 1.18.31) and an npm-latest container. Retired with
# evidence: the control arm never exceeded 19/31 (fixture mismatch + missing
# session seeding — the corpus rides testdata/fixture, the containers mounted
# bench/parity/config), so 11 routes were always classified noise and every
# "drift" verdict inherited that blindness. Behavioral drift of a candidate
# upstream release is now owned by bump.sh's battery A/B *at adoption time*
# (replays under the new pin, red before merge); this script keeps the cheap,
# always-meaningful half: HAS UPSTREAM MOVED?
#
#   - v1: npm `opencode-ai` latest vs the FREEZE_VERSION constant (read from
#     source — one truth, never a copy that can drift from the pin it names).
#   - v2: scripts/v2-drift.sh (npm `@opencode/cli` vs V2_TAG + plugin hook
#     surface diff). Run separately; this script no longer duplicates it.
#
# Usage: scripts/drift-watch.sh [--selftest]
#   --selftest  offline checks of the pure helpers (guard rule 5)
# Exit: 0 = report written (drift found is INFORMATION, not failure)
#       1 = harness failure (npm unreachable / version constant unreadable)
# schedule: ocserve-drift-watch.timer (systemd --user, 06:00, Persistent); or
#   cron `0 6 * * * cd <repo> && scripts/drift-watch.sh >> bench/drift/cron.log 2>&1`
# npm output is untrusted (review L1): validated before it reaches the report.
set -euo pipefail
cd "$(dirname "$0")/.."
umask 077  # reports carry pin values — owner-only (review L5)

REPORT_DIR="${DRIFT_REPORT_DIR:-bench/drift}"

# ─────────────────────────── pure helpers (selftested) ───────────────────────────

# version_gt A B → true iff A > B (semantic-ish, numeric dot segments)
version_gt() {
  [ "$1" != "$2" ] || return 1
  [ "$(printf '%s\n%s\n' "$2" "$1" | sort -V | tail -n 1)" = "$1" ]
}

# valid_semver VALUE → 0 iff a plain x.y.z(.pre). Registry output is untrusted:
# reject anything else before it reaches image tags, files, or the report.
valid_semver() {
  [[ "$1" =~ ^[0-9]+\.[0-9]+\.[0-9]+([.+-][0-9A-Za-z.+-]+)?$ ]]
}

# read_pin FILE CONST NAME → the string inside FILE's `pub const C: &str = "…";`
read_pin() { # file const
  sed -n 's/.*'"$2"'[[:space:]]*:[[:space:]]*&str = "\([^"]*\)".*/\1/p' "$1" | head -n 1
}

# report_write FILE freeze latest verdict
report_write() {
  local file="$1" freeze="$2" latest="$3" verdict="$4"
  mkdir -p "$(dirname "$file")"
  {
    echo "# upstream drift (v1) — $(date -u +%Y-%m-%dT%H:%M:%SZ)"
    echo
    echo "| | |"
    echo "|---|---|"
    echo "| pin (ocserve FREEZE_VERSION) | \`opencode ${freeze}\` |"
    echo "| npm latest (opencode-ai) | \`opencode ${latest}\` |"
    echo "| verdict | **${verdict}** |"
    echo
    echo "Behavioral drift of the latest release is evaluated by \`bump.sh\`'s"
    echo "battery A/B at adoption time (this script's corpus replay was retired:"
    echo "control arm never green — see header). Adoption gate: \`scripts/bump.sh v1 ${latest}\`."
  } >"$file"
}

run_selftest() {
  local bad=0 tmp
  tmp="$(mktemp -d "${TMPDIR:-/tmp}/drift-selftest.XXXXXX")"

  # version_gt: strict ordering pair and the equal case (which must be false —
  # a `>=` drift watcher would report drift on a no-op release)
  version_gt 1.19.5 1.18.31 || { echo "selftest: version_gt proper"; bad=1; }
  version_gt 1.18.31 1.19.5 && { echo "selftest: version_gt rejects reversed"; bad=1; }
  version_gt 1.19.5 1.19.5 && { echo "selftest: version_gt must be strict (equal)"; bad=1; }

  # registry output validation: accept plain semver, reject hostile strings
  valid_semver "1.19.5" || { echo "selftest: valid_semver accepts"; bad=1; }
  valid_semver "1.19.5-rc.1" || { echo "selftest: valid_semver accepts prerelease"; bad=1; }
  valid_semver "1.18.34; touch /tmp/pwned" && { echo "selftest: valid_semver rejects shell meta"; bad=1; }
  valid_semver "" 2>/dev/null && { echo "selftest: valid_semver rejects empty"; bad=1; }

  # read_pin: the exact const line shape spec.rs / lib.rs use
  cat >"$tmp/pin" <<'EOF'
/// doc comment that must not match
pub const FREEZE_VERSION: &str = "1.18.31";
pub const V2_TAG: &str = "2.0.28";
EOF
  [ "$(read_pin "$tmp/pin" FREEZE_VERSION)" = "1.18.31" ] || { echo "selftest: read_pin extracts const"; bad=1; }
  [ -z "$(read_pin "$tmp/pin" OCCURRENCE_OF_NONE)" ] || { echo "selftest: read_pin no match empty"; bad=1; }

  # report writer: structure + verdict row survive a pipe-escape pass
  report_write "$tmp/r.md" 1.18.31 1.19.5 "| behind (6 releases) |"
  grep -q "# upstream drift (v1)" "$tmp/r.md" || { echo "selftest: report header"; bad=1; }
  grep -qF '1.18.31' "$tmp/r.md" || { echo "selftest: report pin row"; bad=1; }

  if [ "$bad" -eq 0 ]; then echo "drift-watch selftest ok"; fi
  return "$bad"
}

# ─────────────────────────── main ───────────────────────────

MODE="${1:-}"
if [ "$MODE" = "--selftest" ]; then
  run_selftest
  exit $?
fi

# Receipt for `ocserve doctor` (scripts/lib/gate-status.sh, rule 61). The trap
# MUST be the last EXIT trap installed (bash keeps only the newest) and must
# call `gate_exit_trap`; a later workdir-cleanup trap would otherwise overwrite
# it and leave the receipt saying `running` forever.
# shellcheck source=scripts/lib/gate-status.sh
. "$(dirname "$0")/lib/gate-status.sh"
DRIFT_DETAIL="exited before completing"
gate_begin drift-watch
gate_exit_trap() {
  local rc=$?
  if [ "$rc" -eq 0 ]; then
    gate_finish drift-watch ok "$DRIFT_DETAIL"
  else
    gate_finish drift-watch failed "exit ${rc}: ${DRIFT_DETAIL}"
  fi
  return $rc
}
trap gate_exit_trap EXIT

command -v npm >/dev/null 2>&1 || { echo "harness failure: npm missing"; exit 1; }

LATEST="$(npm view opencode-ai version 2>/dev/null)" || { echo "harness failure: npm view opencode-ai failed"; exit 1; }
[ -n "$LATEST" ] || { echo "harness failure: npm returned no version"; exit 1; }
valid_semver "$LATEST" || { echo "harness failure: npm version has unexpected format: '$LATEST'"; exit 1; }

PIN_FILE="crates/ocserve-http/src/lib.rs"
FROZEN="$(read_pin "$PIN_FILE" FREEZE_VERSION)" || FROZEN=""
[ -n "$FROZEN" ] || { echo "harness failure: cannot read FREEZE_VERSION from $PIN_FILE"; exit 1; }

# classification
if [ "$LATEST" = "$FROZEN" ]; then
  VERDICT="in line"
elif version_gt "$LATEST" "$FROZEN"; then
  # behind by how many releases? best effort, informational only
  N="$(npm view opencode-ai versions --json 2>/dev/null | python3 -c '
import json,sys
vs=[v for v in json.load(sys.stdin) if v.count(".")==2]
pin=sys.argv[1]; latest=sys.argv[2]
try: print(len(vs[vs.index(pin):vs.index(latest)]))
except ValueError: print("?")' "$FROZEN" "$LATEST" 2>/dev/null || echo "?")"
  VERDICT="behind by ${N} release(s) — adoption gate: \`scripts/bump.sh v1 ${LATEST}\`"
  [ "$N" != "?" ] || VERDICT="behind (count unavailable)"
else
  VERDICT="pin AHEAD of npm latest (pin=${FROZEN} > latest=${LATEST}) — investigate registry state"
fi

STAMP="$(date -u +%Y%m%dT%H%M%SZ)"
REPORT="$REPORT_DIR/${STAMP}-upstream-drift.md"
report_write "$REPORT" "$FROZEN" "$LATEST" "$VERDICT"
echo "v1 upstream drift: pin=${FROZEN} latest=${LATEST} → ${VERDICT}"
echo "report: $REPORT"
DRIFT_DETAIL="pin ${FROZEN} latest ${LATEST}: ${VERDICT}"
exit 0