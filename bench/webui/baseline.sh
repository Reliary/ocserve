#!/usr/bin/env bash
# Web-UI byte + latency baseline (WEBUI-PLAN.md §Baseline).
#
# Usage: bench/webui/baseline.sh [BASE_URL]   (default http://127.0.0.1:4912)
#
# Prints, for the entry HTML and every referenced asset plus the hot API
# routes: identity bytes, gzip bytes, whether Content-Encoding is negotiated,
# ETag presence, and cold latency. Read-only: no writes to any service.
# (No `set -e`: this is a reporting script and a missing header must print
# "none", not abort the run.)
set -u
BASE="${1:-http://127.0.0.1:4912}"
UA='Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/147.0.0.0 Safari/537.36'

row() { # name url ae
  local name="$1" url="$2" ae="$3"
  local out
  if [ -n "$ae" ]; then
    out=$(curl -s -o /dev/null -D - -H "Accept-Encoding: $ae" -H "User-Agent: $UA" \
      -w 'SIZE=%{size_download} TIME=%{time_total} CODE=%{http_code}\n' --max-time 60 "$url" || true)
  else
    out=$(curl -s -o /dev/null -D - -H "User-Agent: $UA" \
      -w 'SIZE=%{size_download} TIME=%{time_total} CODE=%{http_code}\n' --max-time 60 "$url" || true)
  fi
  local enc etag
  enc=$(printf '%s' "$out" | grep -i '^content-encoding:' | tr -d '\r' | awk '{print $2}') || true
  etag=$(printf '%s' "$out" | grep -i '^etag:' | tr -d '\r' | awk '{print $2}') || true
  local stats
  stats=$(printf '%s' "$out" | grep -E '^SIZE=' | tr -d '\r') || true
  printf '%-28s ae=%-8s enc=%-8s etag=%-24s %s\n' "$name" "${ae:-identity}" "${enc:-none}" "${etag:-none}" "$stats"
}

echo "== entry + assets (identity) =="
HTML=$(curl -s --max-time 30 -H "User-Agent: $UA" "$BASE/")
printf '%s' "$HTML" | grep -oE '(src|href)="/assets/[^"]+"' | sed 's/.*="//;s/"//' | sort -u | while read -r a; do
  row "$(basename "$a")" "$BASE$a" ""
done
row "index.html" "$BASE/" ""

echo
echo "== entry + assets (brotli/gzip negotiation) =="
for a in $(printf '%s' "$HTML" | grep -oE '(src|href)="/assets/[^"]+"' | sed 's/.*="//;s/"//' | sort -u); do
  row "$(basename "$a")" "$BASE$a" "br,gzip"
done
row "index.html" "$BASE/" "br,gzip"

echo
echo "== hot API routes (identity) =="
for r in /provider /config /agent /command /config/providers /session /session/status /global/config /vcs /vcs/status /api/health; do
  row "$r" "$BASE$r" ""
done

echo
echo "== hot API routes (gzip negotiation) =="
for r in /provider /session /global/config; do
  row "$r" "$BASE$r" "br,gzip"
done

echo
echo "== revalidation (If-None-Match round-trip on /provider) =="
ETAG=$(curl -s -o /dev/null -D - -H "User-Agent: $UA" "$BASE/provider" | grep -i '^etag:' | tr -d '\r' | awk '{print $2}')
if [ -n "$ETAG" ]; then
  curl -s -o /dev/null -D - -H "User-Agent: $UA" -H "If-None-Match: $ETAG" \
    -w 'revalidate code=%{http_code} size=%{size_download}\n' --max-time 30 "$BASE/provider"
else
  echo "revalidate: no ETag on /provider (expected before W2)"
fi
