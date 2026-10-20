#!/usr/bin/env bash
# Drive the Windows test VM (PLATFORM.md Phase W).
#
# This is deliberately NOT a persistent stage and NOT a CI runner: bring it up
# for a session, take the snapshot, run the oracles, then `down --purge`.
#
# Usage:
#   scripts/windows-vm.sh up          start the VM (first boot installs Windows)
#   scripts/windows-vm.sh wait        block until SSH answers
#   scripts/windows-vm.sh snapshot    copy data.img -> data.snapshot.img
#   scripts/windows-vm.sh rollback    restore the snapshot
#   scripts/windows-vm.sh push        copy the built binary + fixtures into the VM
#   scripts/windows-vm.sh pair        run the v1 pair oracle against both arms
#   scripts/windows-vm.sh ssh [cmd]   run a command in the guest
#   scripts/windows-vm.sh status      container + SSH state
#   scripts/windows-vm.sh down [--purge]
#
# Requires: docker + compose plugin, /dev/kvm, an SSH key in bench/windows/keys.
set -euo pipefail
cd "$(dirname "$0")/.."
W=bench/windows
KEY="$W/keys/id_ed25519"
SSH_OPTS=(-i "$KEY" -p 2222 -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null
          -o ConnectTimeout=10 -o LogLevel=ERROR)

say() { printf '%s\n' "$*"; }
die() { echo "windows-vm: $*" >&2; exit 1; }

compose() { (cd "$W" && docker compose --env-file .env "$@"); }

wait_ssh() {
  local deadline=$(( $(date +%s) + ${1:-2400} ))
  say "waiting for ssh on 127.0.0.1:2222 (first boot installs Windows: 15-40 min)"
  while [ "$(date +%s)" -lt "$deadline" ]; do
    if ssh "${SSH_OPTS[@]}" Docker@127.0.0.1 'echo ready' 2>/dev/null | grep -q ready; then
      say "ssh ready"
      return 0
    fi
    printf '.'
    sleep 10
  done
  echo
  die "ssh never came up; check: docker logs ocserve-windows  (and http://127.0.0.1:8006)"
}

case "${1:-}" in
  up)
    [ -f "$W/.env" ] || die ".env missing (VM_PASSWORD)"
    [ -f "$KEY" ] || die "ssh key missing at $KEY"
    compose up -d
    say "container started; web console http://127.0.0.1:8006"
    say "next: scripts/windows-vm.sh wait"
    ;;
  wait) wait_ssh "${2:-2400}" ;;
  status)
    compose ps || true
    if ssh "${SSH_OPTS[@]}" Docker@127.0.0.1 'ver' 2>/dev/null; then say "ssh ok"; else say "ssh not reachable"; fi
    ;;
  ssh)
    shift
    exec ssh "${SSH_OPTS[@]}" Docker@127.0.0.1 "$@"
    ;;
  snapshot)
    [ -f "$W/vm/data.img" ] || die "no data.img yet (VM not installed?)"
    # The VM must be stopped for a consistent image copy.
    compose stop || true
    cp --reflink=auto "$W/vm/data.img" "$W/vm/data.snapshot.img"
    compose start
    say "snapshot written ($(du -h "$W/vm/data.snapshot.img" | cut -f1))"
    ;;
  rollback)
    [ -f "$W/vm/data.snapshot.img" ] || die "no snapshot"
    compose stop || true
    cp --reflink=auto "$W/vm/data.snapshot.img" "$W/vm/data.img"
    compose start
    say "rolled back; wait for ssh"
    ;;
  push)
    # Everything the guest needs to run the oracles. The comparator itself stays
    # on Linux (it is the same binary used for every other pair run).
    [ -f target/x86_64-pc-windows-msvc/debug/ocserve.exe ] ||
      die "build first: cargo xwin build -p ocserve --target x86_64-pc-windows-msvc"
    ssh "${SSH_OPTS[@]}" Docker@127.0.0.1 'if not exist C:\ocserve mkdir C:\ocserve' || true
    scp -i "$KEY" -P 2222 -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null \
      target/x86_64-pc-windows-msvc/debug/ocserve.exe Docker@127.0.0.1:C:/ocserve/ocserve.exe
    say "pushed ocserve.exe"
    ;;
  pair)
    # Start both arms INSIDE the guest, tunnel their ports to this host, then run
    # the ordinary Linux comparator against the tunnel. There is no
    # Windows-specific comparator, which is the point: the oracle is the same
    # code on every platform, so a divergence means a real divergence.
    #
    # `pair-check.sh` cannot be reused here because it boots its own arms; this
    # invokes the comparator it wraps (`replay --pair`) directly.
    say "starting freeze + ocserve in the guest"
    ssh "${SSH_OPTS[@]}" Docker@127.0.0.1 'powershell -NoProfile -ExecutionPolicy Bypass -File C:\ocserve\start-arms.ps1' \
      || die "start-arms.ps1 failed (see C:\ocserve\*.log in the guest)"

    ssh "${SSH_OPTS[@]}" -N -L 4926:127.0.0.1:4926 -L 4927:127.0.0.1:4927 Docker@127.0.0.1 &
    tun=$!
    trap 'kill $tun 2>/dev/null || true; ssh "${SSH_OPTS[@]}" Docker@127.0.0.1 "powershell -NoProfile -File C:\\ocserve\\stop-arms.ps1" >/dev/null 2>&1 || true' EXIT
    sleep 5

    allow="$PWD/bench/pair/allow.txt"
    rc=0
    if [ -f "$allow" ]; then
      OCSERVE_PAIR_ALLOW="$allow" target/debug/ocserve replay \
        --target http://127.0.0.1:4926 --pair http://127.0.0.1:4927 || rc=$?
    else
      target/debug/ocserve replay \
        --target http://127.0.0.1:4926 --pair http://127.0.0.1:4927 || rc=$?
    fi
    say "windows pair rc=$rc"
    exit "$rc"
    ;;
  down)
    if [ "${2:-}" = "--purge" ]; then
      compose down -v || true
      rm -rf "$W/vm" "$W/shared"/* "$W/keys" "$W/.env"
      say "purged (image kept; remove with: docker rmi dockurr/windows)"
    else
      compose down
      say "stopped (data kept in $W/vm)"
    fi
    ;;
  *)
    sed -n '2,22p' "$0" | sed 's/^# \{0,1\}//'
    exit 2
    ;;
esac
