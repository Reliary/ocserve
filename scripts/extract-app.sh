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
#   4. write a scratch tree, then pack it into the owning crate's assets:
#      crates/ocserve-http/assets/webui/<tag>.pack.zst (+ <tag>.manifest.json)
#
# The pack is committed and embedded at build time. It MUST live inside the
# crate that embeds it — an artifact under `bench/` (outside every crate root)
# is not in the `.crate` tarball, so `cargo install` fails to compile it (the
# 0.3.0 defect; guard rule 30). Re-run on any upstream bump:
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

ASSET_DIR="crates/ocserve-http/assets/webui"
TMP="$(mktemp -d "${TMPDIR:-/tmp}/ocserve-extract.XXXXXX")"
OUT="$TMP/app.$TAG"
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
# Root HTML, plus the browser-probed root assets that are NOT referenced from
# index.html (favicon.ico is auto-requested; sw.js is fetched by the app's own
# service-worker registration inside the bundle). Missing these made ocserve
# serve SPA HTML for /sw.js and /favicon.ico where upstream serves the real
# asset (found by scripts/webui-v2-check.py, 2026-10-18).
queue = ["/", "/sw.js", "/favicon.ico", "/apple-touch-icon.png"]
# Root-referenced files + transitive asset refs discovered in text bodies.
# Both generations: v1 bundles reference /assets/*, v2 bundles /_assets/*
# (plus /icons/* favicons in both). Miss either prefix and the pack ships a
# broken shell (Phase E: the regex is load-bearing).
#
# Dynamic-import chunks (rolldown/vite code-split) are referenced in the entry
# bundle by BARE name (`browser-CCGqgeuQ.js`, `diff-changes-CiKJ-DaB.js`) or a
# relative path (`./chunk.js`), NOT as `/_assets/...` — the browser resolves
# them against the entry's base (`/_assets/`). Capturing only absolute refs
# shipped 131 files and 404'd ~52 lazily-loaded chunks (found by the browser
# acceptance scripts/webui-v2-check.py, 2026-10-18). So also capture bare
# `<name>-<hash>.js`/`.css` tokens and resolve them under the asset dir.
asset_re = re.compile(
    rb'(?:src|href)="(/[^"]+)"'          # absolute index refs
    rb'|(/?_?assets/[A-Za-z0-9_.\-/]+)'  # asset refs in JS/CSS (leading / optional)
    rb'|(/icons/[A-Za-z0-9_.\-/]+)'      # icons
    rb'|([A-Za-z0-9_\-]+-[A-Za-z0-9_\-]{8}\.(?:js|css))'  # bare chunk names
)
# Where bare chunk names resolve (the entry lives at /_assets/, v2) — we try
# both prefixes so v1 (/assets) and v2 (/_assets) both work.
BARE_PREFIXES = ["/_assets/", "/assets/"]


def enqueue(ref: str):
    ref = ref.strip()
    if not ref or ref.startswith(("//", "data:")) or "://" in ref:
        return
    # A relative asset ref (`_assets/x.js`) is served from the site root.
    if not ref.startswith("/"):
        ref = "/" + ref
    if ref not in seen and ref not in queue:
        queue.append(ref)


while queue:
    path = queue.pop(0)
    if path in seen or len(seen) > 3000:
        continue
    try:
        resp = urllib.request.urlopen(base + path, timeout=15)
        body = resp.read()
        ctype = resp.headers.get("content-type", "")
    except Exception as e:
        print(f"  skip {path}: {e}", file=sys.stderr)
        continue
    # A non-root, non-.html path that returns text/html is the SPA fallback,
    # NOT a real asset (v1 has no /sw.js). Store only real assets.
    if path != "/" and not path.endswith(".html") and "text/html" in ctype:
        print(f"  skip {path}: SPA fallback (text/html)", file=sys.stderr)
        continue
    seen[path] = body
    # Discover further local references from text-ish bodies.
    if path.endswith((".js", ".css", ".html", ".json", ".webmanifest", ".svg")) or path == "/":
        for m in asset_re.finditer(body):
            absolute = m.group(1) or m.group(2) or m.group(3)
            if absolute:
                enqueue(absolute.decode("latin1"))
                continue
            bare = m.group(4)  # a bare chunk name → resolve under the asset dir
            if bare:
                name = bare.decode("latin1")
                for pref in BARE_PREFIXES:
                    enqueue(pref + name)

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
rm -rf "$OUT"
mv "$TMP/out" "$OUT"

# Pack the closure into ONE embeddable artifact inside the owning crate. The
# loose files are dropped after packing (vendored third-party assets must not
# live in git); the committed source of truth is the pack + manifest:
#   crates/ocserve-http/assets/webui/<tag>.pack.zst      — embedded (include_bytes!)
#   crates/ocserve-http/assets/webui/<tag>.manifest.json — path → sha256 → bytes
#
# Format (OCAP2, per-file zstd frames), BEFORE the outer... NOTE the outer file
# is ALSO zstd (`zstd -19`). OCAP2 exists so the loader can decompress ONE file
# at a time into a bounded cache instead of materializing the whole closure:
# the full v2 closure is ~87 MB raw (36 MB of lazily-loaded office-preview
# WASM); eager decompress would allocate 87 MB on every boot that selects the
# pack (MEMORY.md bounded-by-construction). Structure:
#   "OCAP2\n" | u32 count |
#     count * (u16 pathlen, path, u64 offset, u32 raw_len, u32 comp_len) |
#     count * (comp_len bytes: an independent zstd frame)
mkdir -p "$ASSET_DIR"
python3 - "$OUT" "$TMP/app.pack" "$ASSET_DIR/$TAG.manifest.json" <<'PY'
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

# per-file frames (independent zstd; small files compress a touch worse than a
# single stream, but the loader gains random access with bounded memory)
frames = []
# per-file frames (independent zstd via the CLI; the loader decompresses ONE
# file at a time into a bounded LRU, so memory is O(1) not O(closure)).
import subprocess
def _zstd(data):
    return subprocess.run(["zstd", "-19", "-q", "-c"], input=data,
                          capture_output=True, check=True).stdout

frames = []
for _rel, data in entries:
    frames.append(_zstd(data))

blob = bytearray(b"OCAP2\n")
blob += struct.pack("<I", len(entries))
manifest = {}
offset = 0
index = []
for (rel, data), frame in zip(entries, frames):
    pb = rel.encode("utf-8")
    index.append((pb, offset, len(data), len(frame)))
    offset += len(frame)
    manifest["/" + rel] = {"sha256": hashlib.sha256(data).hexdigest(), "bytes": len(data)}
for pb, off, raw_len, comp_len in index:
    blob += struct.pack("<H", len(pb)) + pb
    blob += struct.pack("<Q", off) + struct.pack("<I", raw_len) + struct.pack("<I", comp_len)
for frame in frames:
    blob += frame

with open(pack_path, "wb") as f:
    f.write(bytes(blob))
with open(manifest_path, "w") as f:
    json.dump({"tag": os.path.basename(src), "files": manifest}, f, indent=1, sort_keys=True)
print(f"packed {len(entries)} files: raw {sum(len(d) for _,d in entries)} bytes, pack {len(blob)} bytes")
PY

# The OCAP2 blob already carries a per-file zstd frame each, so it is embedded
# AS-IS (no outer zstd — that would force a full decompress at boot to reach
# the index; the loader reads the frames lazily). Copy into place.
cp "$TMP/app.pack" "$ASSET_DIR/$TAG.pack.zst"
stat -c 'pack: %s bytes (OCAP2, per-file zstd frames)' "$ASSET_DIR/$TAG.pack.zst"

# Loose extracted files are regenerable from the pack via the manifest; keep
# the tree small. The committed source of truth is <tag>.pack.zst +
# <tag>.manifest.json (both inside the owning crate).
rm -rf "$OUT"
echo "extract-app: wrote $ASSET_DIR/$TAG.pack.zst + $TAG.manifest.json"
