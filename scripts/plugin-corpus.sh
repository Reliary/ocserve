#!/usr/bin/env bash
# Fetch and run the pinned plugin corpus in a sandbox.
#
# Two stages, because they need opposite things:
#   fetch — needs network; runs on the host and unpacks tarballs WITHOUT running
#           npm lifecycle scripts (postinstall is arbitrary code).
#   run   — executes untrusted plugin code: no network, read-only root,
#           non-root, tmpfs HOME, bounded memory/CPU/PIDs, hard timeout.
#
# The corpus is ephemeral (gitignored). Only the report is committed.
#
# Usage:
#   scripts/plugin-corpus.sh                 # fetch + run locally
#   scripts/plugin-corpus.sh --fetch-only
#   scripts/plugin-corpus.sh --run-only
#   scripts/plugin-corpus.sh --out FILE
#
# Local sandbox needs docker. Remote sandbox: see bench/plugins/README.md
# (the same image and flags are used there; only the host differs).
set -euo pipefail
cd "$(dirname "$0")/.."

FETCH=1
RUN=1
OUT="bench/plugins/report.json"
while [ $# -gt 0 ]; do
  case "$1" in
    --fetch-only) RUN=0; shift ;;
    --run-only) FETCH=0; shift ;;
    --out) OUT="${2:?--out needs a path}"; shift 2 ;;
    *) echo "unknown argument: $1" >&2; exit 2 ;;
  esac
done

CORPUS="bench/plugins/.corpus"
IMAGES=oven/bun:1-alpine

if [ "$FETCH" = 1 ]; then
  echo "== fetch: $(python3 -c 'import json;print(len(json.load(open("bench/plugins/manifest.json"))["plugins"]))') pinned packages, no lifecycle scripts =="
  rm -rf "$CORPUS"
  mkdir -p "$CORPUS"
  python3 - "$CORPUS" <<'PY'
import io, json, os, sys, tarfile, urllib.request

corpus = sys.argv[1]
manifest = json.load(open("bench/plugins/manifest.json"))["plugins"]
UA = {"User-Agent": "ocserve-plugin-corpus"}
for p in manifest:
    name, ver = p["name"], p["version"]
    meta = json.load(
        urllib.request.urlopen(
            urllib.request.Request(
                f"https://registry.npmjs.org/{name.replace('/', '%2f')}", headers=UA
            ),
            timeout=30,
        )
    )
    if ver not in meta["versions"]:
        print(f"MISSING {name}@{ver}")
        continue
    blob = urllib.request.urlopen(
        urllib.request.Request(meta["versions"][ver]["dist"]["tarball"], headers=UA),
        timeout=90,
    ).read()
    dest = os.path.abspath(os.path.join(corpus, name.replace("/", "__")))
    os.makedirs(dest, exist_ok=True)
    with tarfile.open(fileobj=io.BytesIO(blob), mode="r:gz") as tf:
        for member in tf.getmembers():
            top, _, rel = member.name.partition("/")
            if top != "package" or not rel:
                continue
            target = os.path.abspath(os.path.join(dest, rel))
            if not target.startswith(dest + os.sep):
                continue  # refuse ../ escapes
            if member.isdir():
                os.makedirs(target, exist_ok=True)
            elif member.isfile():
                os.makedirs(os.path.dirname(target), exist_ok=True)
                with open(target, "wb") as fh:
                    fh.write(tf.extractfile(member).read())
            # symlinks are skipped on purpose: a package must not point outside
            # its own directory, and the loader refuses that anyway.
    print(f"fetched {name}@{ver}")
PY
  # Install declared dependencies so the plugin is measured as a user would
  # have it. `--ignore-scripts` is essential: a postinstall hook is arbitrary
  # code and would run during fetch (which has network), defeating the sandbox.
  # Some packages ship broken or build-only layouts; those stay load-errors and
  # the report says so.
  if command -v npm >/dev/null 2>&1; then
    python3 bench/plugins/install-deps.py "$CORPUS"
  else
    echo "deps: npm absent; plugins with runtime deps will report load-error"
  fi
  echo "corpus: $(find "$CORPUS" -maxdepth 1 -mindepth 1 -type d | wc -l) packages"
fi

if [ "$RUN" = 1 ]; then
  [ -d "$CORPUS" ] || { echo "no corpus: run without --run-only first" >&2; exit 2; }
  echo "== run: untrusted code, network OFF, bounded =="
  OUTDIR="bench/plugins/.out"
  rm -rf "$OUTDIR"; mkdir -p "$OUTDIR"; chmod 0777 "$OUTDIR"
  docker run --rm \
    --network none \
    --read-only \
    --tmpfs /work:rw,exec,nosuid,size=64m,mode=1777 \
    --tmpfs /tmp:rw,exec,nosuid,size=512m \
    --user 65534:65534 \
    --memory 1g --memory-swap 1g --pids-limit 256 --cpus 1.5 \
    --cap-drop ALL --security-opt no-new-privileges \
    -v "$PWD/bench/plugins/check.mjs:/src/check.mjs:ro" \
    -v "$PWD/bench/plugins/manifest.json:/src/manifest.json:ro" \
    -v "$PWD/crates/ocserve-plugin/src/host:/src/host:ro" \
    -v "$PWD/$CORPUS:/corpus:ro" \
    -v "$PWD/$OUTDIR:/out:rw" \
    -e HOME=/tmp -e XDG_CONFIG_HOME=/tmp/.config -e XDG_DATA_HOME=/tmp/.local/share \
    -w /work \
    "$IMAGES" sh -eu -c '
      # The harness expects a repo-shaped tree; build the smallest one it needs.
      mkdir -p /work/bench/plugins /work/crates/ocserve-plugin/src
      cp /src/check.mjs /src/manifest.json /work/bench/plugins/
      cp -r /src/host /work/crates/ocserve-plugin/src/host
      # The corpus is mounted read-only at its real path; copying 1.2 GB into a
      # tmpfs was the reason this failed with ENOSPC.
      ln -s /corpus /work/bench/plugins/.corpus
      timeout 900 bun bench/plugins/check.mjs --dir /corpus --out /out/report.json
    '
  cp "$OUTDIR/report.json" "$OUT"
  rm -rf "$OUTDIR"
  echo "report: $OUT"
  node -e 'const r=require(process.argv[1]);console.log("summary:",JSON.stringify(r.summary))' "$PWD/$OUT"
fi
