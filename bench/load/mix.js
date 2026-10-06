// Shared route mix for the load suite (L1: reads only — zero provider traffic).
// Every request is tagged with `endpoint` and fed into a per-endpoint Trend
// so the report shows distributions per route (informational); the GATE is
// pooled p95 + error rate (see README claim classes).
import http from 'k6/http';
import { check } from 'k6';
// k6 v2 moved Trend out of the 'k6' barrel (it LINKS there but evaluates to
// null — every run died with "Value is not an object" at `new Trend`; the
// canonical k6/metrics path works, probed live on the .227 runner)
import { Trend } from 'k6/metrics';

export const BASE = __ENV.LOAD_BASE || 'http://127.0.0.1:4930';
export const MODE = __ENV.LOAD_MODE || 'spread'; // spread | hot
export const SIDS = (__ENV.LOAD_SIDS || '').split(',').filter(Boolean);
export const DEEP = __ENV.LOAD_DEEP || SIDS[0] || '';
export const FILE_PATH = __ENV.LOAD_FILE_PATH || '/tmp';
export const SEARCH_Q = __ENV.LOAD_SEARCH_Q || 'the';

export const ENDPOINTS = [
  'session_list',
  'message_page',
  'message_cursor',
  'config',
  'agent',
  'command',
  'file_list',
  'search',
  'session_status',
];

const trends = {};
for (const e of ENDPOINTS) trends[e] = new Trend('lat_' + e, true);

// Per-VU state: module scope is per-VU in k6 (each VU its own runtime).
let cursor = null;
let lastSid = null;

function sidForVU() {
  if (MODE === 'hot') return DEEP;
  const pool = SIDS.length ? SIDS : [DEEP];
  return pool[(__VU - 1) % pool.length];
}

function req(endpoint, method, path, body) {
  const tags = { endpoint };
  const opts = body
    ? { headers: { 'Content-Type': 'application/json' }, tags }
    : { tags };
  const res =
    method === 'POST'
      ? http.post(BASE + path, JSON.stringify(body), opts)
      : http.get(BASE + path, opts);
  trends[endpoint].add(res.timings.duration, { status: String(res.status) });
  check(res, { [endpoint + '_2xx']: (r) => r.status >= 200 && r.status < 300 });
  return res;
}

// One mixed iteration. Sid pinned per VU in spread mode (concurrent
// SESSIONS), deep session in hot mode (contention on the 32k-msg session).
export function doIteration() {
  const sid = sidForVU();
  if (sid !== lastSid) {
    cursor = null; // new session scope → paging restarts
    lastSid = sid;
  }
  switch (__ITER % ENDPOINTS.length) {
    case 0:
      req('session_list', 'GET', '/session?limit=500');
      break;
    case 1: {
      const r = req('message_page', 'GET', `/session/${sid}/message?limit=50`);
      const next = r.headers['X-Next-Cursor'] || r.headers['x-next-cursor'];
      cursor = next || null;
      break;
    }
    case 2:
      if (cursor) {
        req('message_cursor', 'GET', `/session/${sid}/message?limit=50&before=${cursor}`);
      } else {
        req('message_page', 'GET', `/session/${sid}/message?limit=50`);
      }
      break;
    case 3:
      req('config', 'GET', '/config');
      break;
    case 4:
      req('agent', 'GET', '/agent');
      break;
    case 5:
      req('command', 'GET', '/command');
      break;
    case 6:
      req('file_list', 'GET', `/file?path=${encodeURIComponent(FILE_PATH)}`);
      break;
    case 7:
      req('search', 'POST', '/session/search', { query: SEARCH_Q, limit: 50 });
      break;
    case 8:
      // GLOBAL status (W2 golden {}|busy map). The per-session variant
      // does NOT exist on either server: refine 404s honestly (unregistered
      // route), freeze returns 200 SPA catch-all HTML — a status-code-only
      // check counted that as a pass on run1 (recorded lesson).
      req('session_status', 'GET', '/session/status');
      break;
    default:
      break;
  }
}

// Gated mode: pooled error-rate + pooled p95 thresholds (env-provided —
// numbers come from thresholds.json AFTER the informational baseline run;
// see README). Baseline runs pass --no-thresholds so options are inert.
export function buildThresholds() {
  const th = {};
  if (__ENV.LOAD_ERR_MAX) th.http_req_failed = ['rate<' + Number(__ENV.LOAD_ERR_MAX)];
  if (__ENV.LOAD_P95_MAX) th.http_req_duration = ['p(95)<' + Number(__ENV.LOAD_P95_MAX)];
  return th;
}

// Ramp ladder from LOAD_TARGETS (comma-separated concurrency levels): ramp
// to each target, hold, then ramp down — the concurrency capacity curve.
export function rampStages() {
  const targets = (__ENV.LOAD_TARGETS || '10,25')
    .split(',')
    .map((x) => Number(x.trim()))
    .filter((n) => n > 0);
  const stages = [];
  for (const t of targets) {
    stages.push({ duration: '20s', target: t });
    stages.push({ duration: '45s', target: t });
  }
  stages.push({ duration: '10s', target: 0 });
  return stages;
}
