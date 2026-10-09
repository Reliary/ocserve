#!/usr/bin/env bash
# Extract the version-matched web-UI asset closure from an opencode build.
#
# Upstream's `serveUIEffect` is embedded-first: the release binary embeds the
# whole `packages/app/dist` and serves it at `/` + `/assets/*`, proxying to
# app.opencode.ai only when embedding is disabled. ocserve historically only
# had the proxy — so it served a NEWER Cloudflare UI against the frozen server
# (version skew). This script captures the frozen build's own UI through its
# PUBLIC HTTP interface (no binary scraping, no JS toolchain):
#
#   1. boot the freeze binary on a throwaway port
#   2. GET / → parse every src/href (entry JS + CSS + favicons + manifest)
#   3. transitively GET every asset each references (CSS fonts, worker JS)
#   4. write bench/webui/app/<tag>/ + a manifest (path → sha256, count)
#
# The output is committed and embedded at build time (see ocserve-http/doc.rs
# precedent). Re-run on any upstream bump:
#   scripts/extract-app.sh <freeze-binary> <tag>
#
# Safety: boots a read-only server on a loopback port with an empty HOME; never
# touches the live service or config (AGENTS §11).
set -euo pipefail
cd "$(dirname "$0")/.."

BIN="${1:?usage: extract-app.sh <freeze-binary> <tag>}"
TAG="${2:?usage: extract-app.sh <freeze-binary> <tag>}"
PORT="${EXTRACT_PORT:-5139}"
[ -x "$BIN" ] || { echo "extract-app: $BIN not executable" >&2; exit 2; }

ver="$("$BIN" --version 2>/dev/null | head -1)"
case "$ver" in
  *"$TAG"*|"$TAG") ;; # accept a bare tag match
  *) echo "extract-app: binary reports '$ver'; expected tag '$TAG'" >&2; exit 2 ;;
esac

OUT="bench/webui/app/$TAG"
TMP="$(mktemp -d "${TMPDIR:-/tmp}/ocserve-extract.XXXXXX")"
cleanup() {
  [ -n "${PID:-}" ] && kill -- -"$PID" 2>/dev/null || true
  pkill -f "opencode serve --port $PORT" 2>/dev/null || true
  rm -rf "$TMP"
}
trap cleanup EXIT

mkdir -p "$TMP/home/.local/share/opencode" "$TMP/home/.config/opencode"
setsid env HOME="$TMP/home" OPENCODE_DISABLE_MODELS_FETCH=1 \
  "$BIN" serve --port "$PORT" --hostname 127.0.0.1 >"$TMP/serve.log" 2>&1 &
PID=$!

for _ in $(seq 1 60); do
  curl -s --max-time 1 "http://127.0.0.1:$PORT/global/health" >/dev/null 2>&1 && break
  kill -0 "$PID" 2>/dev/null || { echo "extract-app: server died"; cat "$TMP/serve.log"; exit 2; }
  sleep 0.5
done

# Python does the crawl (bounded, dedup, transitive).
python3 - "$PORT" "$TMP/out" "$TAG" <<'PY'
import hashlib, json, os, re, sys, urllib.request

port, out, tag = sys.argv[1], sys.argv[2], sys.argv[3]
base = f"http://127.0.0.1:{port}"
os.makedirs(out, exist_ok=True)

def get(path):
    return urllib.request.urlopen(base + path, timeout=15).read()

seen = {}
queue = ["/"]
# Root-referenced files + transitive /assets refs discovered in text bodies.
asset_re = re.compile(rb'(?:src|href)="(/[^"]+)"|(/assets/[A-Za-z0-9_.-]+)')
while queue:
    path = queue.pop(0)
    if path in seen or len(seen) > 400:
        continue
    try:
        body = get(path)
    except Exception as e:
        print(f"  skip {path}: {e}", file=sys.stderr)
        continue
    seen[path] = body
    # Discover further local references from text-ish bodies.
    if path.endswith((".js", ".css", ".html", ".json", ".webmanifest", ".svg")) or path == "/":
        for m in asset_re.finditer(body):
            ref = (m.group(1) or m.group(2) or b"").decode("latin1")
            if not ref or ref.startswith("//") or "://" in ref or ref.startswith("data:"):
                continue
            if ref not in seen and ref not in queue:
                queue.append(ref)

# Write files under the tag dir (path → relative file, leading / stripped).
manifest = {}
for path, body in sorted(seen.items()):
    # "/" serves index.html (the SPA root); store it as index.html.
    rel = "index.html" if path == "/" else path.lstrip("/")
    dst = os.path.join(out, rel)
    os.makedirs(os.path.dirname(dst), exist_ok=True)
    with open(dst, "wb") as f:
        f.write(body)
    manifest[path] = {"sha256": hashlib.sha256(body).hexdigest(), "bytes": len(body)}

with open(os.path.join(out, "MANIFEST.json"), "w") as f:
    json.dump({"tag": tag, "files": manifest}, f, indent=1, sort_keys=True)
total = sum(v["bytes"] for v in manifest.values())
print(f"extracted {len(manifest)} files, {total} bytes")
PY

# move into place (python wrote to TMP; the tag arg was passed positionally)
mkdir -p "$(dirname "$OUT")"
rm -rf "$OUT" "$OUT.tmp"
mv "$TMP/out" "$OUT"

# Pack the closure into ONE embeddable artifact. The loose files are
# gitignored (10 MB of vendored third-party assets); the committed source of
# truth is the pack + manifest:
#   bench/webui/app/<tag>.pack.zst      — the embedded blob (include_bytes!)
#   bench/webui/app/<tag>.manifest.json — path → sha256 → bytes (inspectable)
# Pack format (before zstd): "OCAP1\n" | u32 count | (u16 pathlen, path,
# u64 datalen, data)* — trivial to parse, no tar dependency.
python3 - "$OUT" "$TMP/app.pack" "bench/webui/app/$TAG.manifest.json" <<'PY'
import hashlib, json, os, struct, sys

src, pack_path, manifest_path = sys.argv[1], sys.argv[2], sys.argv[3]
entries = []
for root, _dirs, files in os.walk(src):
    for name in sorted(files):
        full = os.path.join(root, name)
        rel = os.path.relpath(full, src).replace(os.sep, "/")
        if rel == "MANIFEST.json":
            continue  # the crawl's own manifest; the pack manifest supersedes
        with open(full, "rb") as f:
            data = f.read()
        entries.append((rel, data))
entries.sort()

blob = bytearray(b"OCAP1\n")
blob += struct.pack("<I", len(entries))
manifest = {}
for rel, data in entries:
    pb = rel.encode("utf-8")
    blob += struct.pack("<H", len(pb)) + pb
    blob += struct.pack("<Q", len(data)) + data
    manifest["/" + rel] = {"sha256": hashlib.sha256(data).hexdigest(), "bytes": len(data)}

with open(pack_path, "wb") as f:
    f.write(bytes(blob))
with open(manifest_path, "w") as f:
    json.dump({"tag": os.path.basename(src), "files": manifest}, f, indent=1, sort_keys=True)
print(f"packed {len(entries)} files: raw {len(blob)} bytes")
PY

# zstd the raw pack (CLI; -19, deterministic content). This is the embedded blob.
zstd -19 -q -f "$TMP/app.pack" -o "bench/webui/app/$TAG.pack.zst"
stat -c 'pack: %s bytes (zstd)' "bench/webui/app/$TAG.pack.zst"

# Loose extracted files are regenerable from the pack via the manifest; keep
# the tree small (10 MB of vendored assets must not live in git). The
# committed source of truth is <tag>.pack.zst + <tag>.manifest.json.
printf '%s\n' \
  "# Loose extracted assets are regenerable (scripts/extract-app.sh). The" \
  "# committed source of truth is <tag>.pack.zst + <tag>.manifest.json." \
  "1.18.31/" > bench/webui/app/.gitignore
rm -rf "$OUT"
echo "extract-app: wrote $TAG.pack.zst + $TAG.manifest.json (loose files removed)"
