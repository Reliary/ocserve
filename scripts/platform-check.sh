#!/usr/bin/env bash
# Local reproduction of the CI `platform check` job, including its compiler
# flags. The flags are the point: `actions-rust-lang/setup-rust-toolchain` sets
# `RUSTFLAGS=-D warnings` by default, so a warning locally is an error in CI.
# The first version of this check ran without them and passed while CI failed
# (PLATFORM.md §1.2 — the "unused import in a cfg-gated test module" round trip).
#
# Usage: scripts/platform-check.sh [target]
#   default target: x86_64-pc-windows-msvc
#
# Requires cargo-xwin (`cargo install cargo-xwin --locked`); it fetches the MSVC
# CRT + Windows SDK headers, which is why plain `cargo check --target` fails on
# the C dependencies (ring, zstd-sys, libsqlite3-sys).
set -euo pipefail
cd "$(dirname "$0")/.."

TARGET="${1:-x86_64-pc-windows-msvc}"
[ -x "$(command -v cargo-xwin)" ] || {
  echo "platform-check: cargo-xwin missing (cargo install cargo-xwin --locked)" >&2
  exit 2
}

echo "platform-check: $TARGET with -D warnings"
RUSTFLAGS="-D warnings" cargo xwin check --workspace --all-targets --target "$TARGET"
echo "platform-check: ok"
