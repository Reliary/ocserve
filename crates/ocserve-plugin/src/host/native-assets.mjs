/**
 * Resolve native assets that a bundled plugin still loads from its original
 * package tree. The normalizer moves ESM outside node_modules; Bun FFI assets
 * remain beside the cached package and cannot be bundled into JavaScript.
 */
import { readFileSync, statSync } from "node:fs";
import { dirname, join, resolve } from "node:path";

// A plugin entry can be nested under dist/ or src/. Keep the upward search
// explicitly bounded so a malformed origin cannot turn loading into a walk of
// the whole filesystem.
const MAX_ORIGIN_PARENTS = 12;

// bun-pty prefers the `_musl` builds on musl libc (Alpine); the glibc names are
// its fallback.
//
// Detect musl the way bun-pty ITSELF does (`src/terminal.ts: isMusl`) — by
// grepping /proc/self/maps for `ld-musl` — and not via
// `process.report.getReport()`. Measured under bun 1.3.14 on glibc: the report
// object exists but `header.glibcVersionRuntime` is undefined, so the
// report-based heuristic answers "musl" on a glibc host and would hand FFI the
// musl build. If /proc is unreadable, assume glibc: picking the glibc library
// on a musl host fails loudly at dlopen, while picking musl on glibc fails the
// same way, so the tie-break does not matter — but the common case must be
// right.
function isMusl() {
  if (process.platform !== "linux") return false;
  try {
    return readFileSync("/proc/self/maps", "utf8").includes("ld-musl");
  } catch {
    return false;
  }
}

function libraryNames(platform, arch, musl = false) {
  if (platform === "darwin") {
    return arch === "arm64"
      ? ["librust_pty_arm64.dylib", "librust_pty.dylib"]
      : ["librust_pty.dylib"];
  }
  if (platform === "win32") {
    // bun-pty ships BOTH windows builds (rust_pty_arm64.dll exists in
    // 0.4.11's rust-pty/target/release); listing only the x64 name made an
    // arm64 Windows host resolve a library with the wrong architecture.
    return arch === "arm64"
      ? ["rust_pty_arm64.dll", "rust_pty.dll"]
      : ["rust_pty.dll"];
  }
  if (platform === "linux") {
    const glibc =
      arch === "arm64"
        ? ["librust_pty_arm64.so", "librust_pty.so"]
        : ["librust_pty.so"];
    if (!musl) return glibc;
    const muslNames =
      arch === "arm64"
        ? ["librust_pty_arm64_musl.so", "librust_pty_musl.so"]
        : ["librust_pty_musl.so"];
    return [...muslNames, ...glibc];
  }
  return [];
}

export function findBunPtyLib(
  origin,
  platform = process.platform,
  arch = process.arch,
  musl = platform === "linux" && isMusl(),
) {
  if (typeof origin !== "string" || origin.length === 0) return null;
  const names = libraryNames(platform, arch, musl);
  if (names.length === 0) return null;

  let current = dirname(resolve(origin));
  for (let parent = 0; parent < MAX_ORIGIN_PARENTS; parent += 1) {
    for (const name of names) {
      const candidate = join(
        current,
        "node_modules",
        "bun-pty",
        "rust-pty",
        "target",
        "release",
        name,
      );
      try {
        if (statSync(candidate).isFile()) return candidate;
      } catch {
        // The package can contain only some platform builds. Keep looking.
      }
    }
    const parentPath = dirname(current);
    if (parentPath === current) break;
    current = parentPath;
  }
  return null;
}

// The value the operator launched the host with. A path the host set for an
// earlier plugin must not stick for the next plugin's own package tree.
const OPERATOR_BUN_PTY_LIB = process.env.BUN_PTY_LIB;

export function configureBunPtyLib(
  origin,
  env = process.env,
  platform = process.platform,
  arch = process.arch,
  operator = OPERATOR_BUN_PTY_LIB,
) {
  if (typeof operator === "string" && operator.length > 0) {
    env.BUN_PTY_LIB = operator;
    return operator;
  }
  const candidate = findBunPtyLib(origin, platform, arch);
  if (candidate) env.BUN_PTY_LIB = candidate;
  else delete env.BUN_PTY_LIB;
  return candidate;
}
