#!/usr/bin/env python3
"""Live v2 web-UI browser acceptance (GAPS-PLAN Tier 1.3).

Boots ocserve with OCSERVE_UI_PACK=2.0.28 (the embedded 2.0.28 web shell) and
drives it with a real headless Chromium via Playwright: loads `/`, waits for the
SPA to mount, and asserts:

  - the document renders (title + a mounted root node),
  - the page's own network calls to the ocserve API return JSON (not HTML),
  - no uncaught JS error / no failed module load.

This mirrors the v1 web-UI live acceptance and turns the 2.0.28 pack from
"asset-level verified" into "browser-verified". Manual/nightly.

Usage: scripts/webui-v2-check.py <base-url>   (server must already run with
       OCSERVE_UI_PACK=2.0.28)
Exit 0 on green; 1 on any HTML API response or JS error; 2 on boot/timeout.
"""
import sys
import json

try:
    from playwright.sync_api import sync_playwright
except ImportError:
    # Missing dev tool is a SKIP, not a FAIL (nightly convention). Exit 77 so
    # the caller can distinguish "skipped" from "passed"/"failed".
    print("webui-v2-check: SKIPPED (python 'playwright' not installed)")
    sys.exit(77)

BASE = sys.argv[1] if len(sys.argv) > 1 else "http://127.0.0.1:5123"


def main() -> int:
    api_calls = []       # (url, status, content_type)
    api_html = []        # api calls that returned HTML (the crash class)
    failed = []          # (url, status) for any 4xx/5xx response
    js_errors = []

    with sync_playwright() as p:
        browser = p.chromium.launch(args=["--no-sandbox"])
        page = browser.new_page()

        page.on("console", lambda m: js_errors.append(m.text) if m.type == "error" else None)
        page.on("pageerror", lambda e: js_errors.append(str(e)))

        def on_response(resp):
            url = resp.url
            if not url.startswith(BASE):
                return
            path = url[len(BASE):]
            if resp.status >= 400:
                failed.append((path, resp.status))
            if path.startswith("/_assets") or path == "/":
                return
            ct = resp.headers.get("content-type", "")
            api_calls.append((path, resp.status, ct))
            if "text/html" in ct:
                api_html.append((path, resp.status, ct))

        page.on("response", on_response)

        try:
            # `networkidle` never fires: the SPA opens a long-lived SSE stream.
            page.goto(BASE + "/", wait_until="domcontentloaded", timeout=30000)
        except Exception as e:
            print(f"webui-v2-check: navigation failed: {e}", file=sys.stderr)
            browser.close()
            return 2

        # give the SPA a moment to mount + fire its bootstrap requests
        page.wait_for_timeout(4000)
        title = page.title()
        # a mounted root: #root or a known shell container has children
        mounted = page.evaluate(
            "() => { const r = document.getElementById('root') "
            "|| document.querySelector('[data-component], main, #app'); "
            "return !!(r && r.childElementCount > 0); }"
        )
        browser.close()

    print(f"== title: {title!r}; mounted: {mounted}; api calls: {len(api_calls)} ==")
    for path, status, ct in api_calls[:12]:
        print(f"   {status} {ct:24} {path}")

    # Real failures: 4xx/5xx on a non-asset path (assets 404ing is the pack
    # gap; API 4xx is the shape gap).
    if failed:
        print(f"FAIL: {len(failed)} route(s) returned 4xx/5xx:")
        for path, status in sorted(set(failed))[:20]:
            print(f"   {status} {path}")
        return 1
    if api_html:
        print(f"FAIL: {len(api_html)} API route(s) returned HTML (the crash class):")
        for path, status, ct in api_html:
            print(f"   {status} {ct} {path}")
        return 1
    if not mounted:
        print("FAIL: the v2 SPA did not mount a root node")
        return 1
    if js_errors:
        print(f"FAIL: {len(js_errors)} uncaught JS error(s):")
        for e in js_errors[:8]:
            print(f"   {e}")
        return 1
    print("ok: v2 web UI mounted; all API calls JSON; no JS errors")
    return 0


if __name__ == "__main__":
    sys.exit(main())
