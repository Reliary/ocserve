#!/usr/bin/env bash
# extract-spec.sh <tag> — vendor an OpenAPI contract.
#
# Two sources, two very different pipelines (do not conflate them):
#
#   v1 line (e.g. 1.18.31): the frozen server ships its full OpenAPI 3.1.0
#   document at `GET /doc`; the same document lives in the upstream tree at
#   `packages/sdk/openapi.json`. We vendor it verbatim.
#
#   v2 line (e.g. 2.0.28): the packages ship COMPILED code, not the contract.
#   `packages/protocol/openapi.json` in the v2 tree is stale (verified
#   2026-10-13: 136 ops vendored vs 140 declared). The contract must be
#   REGENERATED with upstream's own pipeline — `OpenApi.fromApi(ClientApi)`
#   plus the stabilize step (`packages/protocol/script/generate-openapi.ts`
#   + `openapi-stabilize.ts`). The regen needs the PUBLISHED npm tarballs
#   `@opencode/protocol@<tag>` + `@opencode/schema@<tag>` plus the effect
#   version the packages declare (4.0.1; NOT effect@3, NOT @effect/platform —
#   `dist/api.js` imports `effect/http-api`, which only exists in 4.x).
#   npm prunes side-loaded packages on every install, so the tarballs must be
#   installed as real (file:) dependencies or re-extracted after install.
#   The stabilize step is content-addressed (sha256 of canonical shape, first
#   12 hex) — a naive regen without it produces phantom diffs.
#
# Usage:
#   scripts/extract-spec.sh 1.18.31 [--from-live URL]
#   scripts/extract-spec.sh 2.0.28 --regen
#
#   (default) reads packages/sdk/openapi.json at the given tag from the
#             local opencode worktree (OPENCODE_SRC) or a fresh shallow fetch.
#   --from-live  copies the document a running server returns at $URL/doc
#             (use to re-verify a vendor against the frozen binary).
#   --regen   regenerates the v2 document from the published npm tarballs
#             with upstream's own pipeline (see above). Verified 2026-10-13:
#             118 paths / 142 ops / 303 schemas for 2.0.28 (vs 113/136 in the
#             stale checked-in file). Adds 6 ops incl. credential list+create,
#             pair, external-connect, vcs/init.
#
# Output is pretty-printed and written to the owning crate's assets directory,
# `crates/ocserve-core/assets/openapi/<tag>.json`. It MUST live inside a crate
# so it is packaged and published with it — an artifact embedded from outside
# a crate root (`bench/`) is not in the `.crate` tarball, so `cargo install`
# fails to compile it (the 0.3.0 defect; guard rule 30). The file is a
# generated artifact — never hand-edit it.
set -euo pipefail

cd "$(dirname "$0")/.."
TAG="${1:?usage: extract-spec.sh <tag> [--from-live URL | --regen]}"
OUT="crates/ocserve-core/assets/openapi/${TAG}.json"
mkdir -p "$(dirname "$OUT")"

if [ "${2:-}" = "--regen" ]; then
  # v2 contract regeneration from published npm tarballs.
  WORK="$(mktemp -d)"
  trap 'rm -rf "$WORK"' EXIT
  echo "fetching @opencode/protocol@${TAG} + @opencode/schema@${TAG}"
  ( cd "$WORK" && npm pack "@opencode/protocol@${TAG}" "@opencode/schema@${TAG}" >/dev/null 2>&1 )
  mkdir -p "$WORK/pkgs/protocol" "$WORK/pkgs/schema"
  tar xzf "$WORK"/opencode-protocol-*.tgz -C "$WORK/pkgs/protocol" --strip-components=1
  tar xzf "$WORK"/opencode-schema-*.tgz -C "$WORK/pkgs/schema" --strip-components=1
  EFFECT="$(node -p "require('$WORK/pkgs/protocol/package.json').dependencies.effect")"
  ( cd "$WORK/pkgs/protocol" && npm install --no-save --no-audit --no-fund \
      "effect@${EFFECT}" "$WORK/pkgs/schema" >/dev/null 2>&1 )
  ( cd "$WORK/pkgs/schema" && npm install --no-save --no-audit --no-fund \
      "effect@${EFFECT}" >/dev/null 2>&1 )
  # NOTE: node must run with cwd inside the protocol package so bare
  # specifiers ('effect/http-api') resolve from its node_modules. The regen
  # program is written to a file because a multi-line `node -e` breaks the
  # surrounding if/elif parsing.
  cat > "$WORK/regen.mjs" <<'NODEEOF'
import { createRequire } from 'node:module';
const require = createRequire(process.cwd() + '/package.json');
const clientPath = require('node:path').resolve(process.cwd(), 'dist/client.js');
const c = await import(clientPath);
const h = await import('effect/http-api');
const fs = await import('fs');
const crypto = await import('crypto');
  const document = h.OpenApi.fromApi(c.ClientApi);
  const ref = '#/components/schemas/';
  const schemas = document.components.schemas;
  const families = ['Union','Objects','Arrays'];
  const rx = (f) => new RegExp('^' + f + '_\\\\d*\$');
  const anonymous = new Map(Object.keys(schemas).filter((n) => families.some((f) => rx(f).test(n))).map((n) => [n, schemas[n]]));
  const canonical = (node, seen = new Set()) => {
    if (Array.isArray(node)) return node.map((x) => canonical(x, seen));
    if (typeof node !== 'object' || node === null) return node;
    const r = ('\$ref' in node && typeof node['\$ref'] === 'string') ? node['\$ref'] : undefined;
    const name = (r && r.startsWith(ref)) ? r.slice(ref.length) : undefined;
    if (name !== undefined && anonymous.has(name)) {
      if (seen.has(name)) throw new Error('recursive anonymous component: ' + name);
      const rest = Object.entries(node).filter(([k]) => k !== '\$ref').sort().map(([k, v]) => [k, canonical(v, seen)]);
      return Object.fromEntries([['\$ref', canonical(anonymous.get(name), new Set([...seen, name]))], ...rest]);
    }
    return Object.fromEntries(Object.entries(node).sort().map(([k, v]) => [k, canonical(v, seen)]));
  };
  const renames = new Map([...anonymous].map(([name, schema]) => {
    const hash = crypto.createHash('sha256').update(JSON.stringify(canonical(schema))).digest('hex').slice(0, 12);
    return [name, name.replace(/_\d*\$/, '') + '_' + hash];
  }));
  const rewrite = (node) => {
    if (Array.isArray(node)) return node.map(rewrite);
    if (typeof node !== 'object' || node === null) return node;
    return Object.fromEntries(Object.entries(node).map(([k, v]) => {
      if (k !== '\$ref' || typeof v !== 'string' || !v.startsWith(ref)) return [k, rewrite(v)];
      return [k, ref + (renames.get(v.slice(ref.length)) ?? v.slice(ref.length))];
    }));
  };
  const result = rewrite(document);
  const stable = new Map();
  for (const [name, schema] of Object.entries(result.components.schemas)) {
    const target = renames.get(name) ?? name;
    const prev = stable.get(target);
    if (prev !== undefined && JSON.stringify(canonical(prev)) !== JSON.stringify(canonical(schema)))
      throw new Error('content-addressed collision: ' + target);
    stable.set(target, schema);
  }
  result.components.schemas = Object.fromEntries([...stable].sort());
  fs.writeFileSync(process.env.OCSERVE_OUT, JSON.stringify(result, null, 2) + '\n');
  console.log('  regen: paths=' + Object.keys(result.paths || {}).length
    + ' schemas=' + Object.keys(result.components.schemas).length);
NODEEOF
  # NOTE: the regen file must live INSIDE the protocol package dir: node
  # resolves bare specifiers ('effect/http-api') from the importing file's
  # location, and /tmp is outside every node_modules tree.
  cp "$WORK/regen.mjs" "$WORK/pkgs/protocol/regen.ocserve.mjs"
  ( cd "$WORK/pkgs/protocol" && OCSERVE_OUT="$OLDPWD/$OUT" node regen.ocserve.mjs )
elif [ "${2:-}" = "--from-live" ]; then
  URL="${3:?--from-live needs a base URL, e.g. --from-live http://127.0.0.1:4901}"
  echo "fetching ${URL}/doc -> ${OUT}"
  curl -fsS --max-time 30 "${URL}/doc" | python3 -m json.tool > "${OUT}"
else
  SRC="${OPENCODE_SRC:-$HOME/src/opencode-v1}"
  if [ -d "$SRC/.git" ] && git -C "$SRC" rev-parse -q --verify "refs/tags/v${TAG}" >/dev/null 2>&1; then
    echo "reading ${SRC}@v${TAG}:packages/sdk/openapi.json -> ${OUT}"
    git -C "$SRC" show "v${TAG}:packages/sdk/openapi.json" | python3 -m json.tool > "${OUT}"
  else
    echo "no local tag v${TAG} in ${SRC}; fetching raw from GitHub"
    curl -fsS --max-time 30 \
      "https://raw.githubusercontent.com/sst/opencode/v${TAG}/packages/sdk/openapi.json" \
      | python3 -m json.tool > "${OUT}"
  fi
fi

python3 - "$OUT" <<'PY'
import json, sys
p = sys.argv[1]
d = json.load(open(p))
ops = sum(len([m for m in v if m in ("get","post","put","delete","patch")]) for v in d["paths"].values())
print(f"  openapi={d.get('openapi')} paths={len(d['paths'])} ops={ops} schemas={len(d.get('components',{}).get('schemas',{}))}")
PY
echo "wrote ${OUT}"

# The v2 gate table is derived from the two vendored contracts, so it must be
# regenerated whenever either one moves. Guard rule 42 fails the build on drift.
if [ -f scripts/gen-v2-gate.py ]; then
  python3 scripts/gen-v2-gate.py
fi
