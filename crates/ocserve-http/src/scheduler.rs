//! Scheduled session wake-ups — the firing half of v2 `experimental.schedule.*`.
//!
//! The routes landed in the 2.2.5 tier as storage only: a schedule was created,
//! listed and transitionable but **never ran**, which is shape-without-mutation
//! and the exact class this repo rejects (a user sets a wake-up in the web UI
//! and nothing happens, with no error anywhere). This module is the runner.
//!
//! Semantics follow upstream `packages/core/src/schedule.ts` (2.2.5):
//!
//! * A due row is woken by ADMITTING a "steer" prompt into the session inbox
//!   with a deterministic message id `msg_<scheduleID>_<run>`, so a retry after
//!   a crash reconciles with the earlier admission instead of double-posting.
//!   The prompt carries `metadata.schedule` (id, run, dueAt, late).
//! * `every` keeps its PHASE: every missed slot is skipped rather than fired in
//!   a burst; `cron` moves to the next slot strictly after now; `in`/`at` are
//!   one-shots.
//! * A firing more than 60 s past due is recorded as `late` (still fires).
//! * `ends.after` / `ends.at` bound the run; `ends.times` decrements
//!   `remainingRuns`; a row with no next slot, no runs left, or a next slot past
//!   its end is `done`.
//! * If the previous admitted prompt is still pending, re-wake the session and
//!   skip this slot (a lost wake is harmless; a duplicate prompt is not).
//! * A failed turn backs off 5 minutes instead of retrying every tick.
//!
//! Deliberate divergences (PLAN §18):
//!   * `D-SCHEDULE-CHECK` — `if`/`until` CONDITION checks are stored and shown
//!     but not evaluated: they need upstream's codemode sandbox. Such a row
//!     behaves as "no condition" and says so in the ledger.
//!   * `D-SCHEDULE-LEADER` — no lease row. ocserve is one process per data dir
//!     and its writer is already the serialization point, so a leader election
//!     would be dead weight; upstream needs it because auxiliary processes
//!     share a directory.
//!   * `D-SCHEDULE-DISABLED` — the flag is read from OUR loaded config (one
//!     set per process), not per session location like upstream.
//!
//! The loop never holds a session lock: it admits into the inbox and wakes the
//! coordinator, so a schedule can never run concurrently with a user turn and
//! cannot outlive the same abort machinery as any other turn.

use crate::AppState;
use serde_json::{Value, json};
use std::sync::Arc;
use std::time::Duration;

/// How far past its due time a firing still counts as on time.
const LATE_AFTER_MS: i64 = 60_000;
/// Retry delay after a failed prompt.
const FAILED_RETRY_MS: i64 = 5 * 60_000;
/// The loop re-reads the table at least this often (clock jumps, lost wakes).
const MAX_SLEEP_MS: i64 = 30_000;
/// Minimum interval between passes.
const MIN_SLEEP_MS: i64 = 1_000;

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Start the runner. Spawned once from `serve`; the flag is re-read each pass,
/// so enabling `experimental.schedules` through a config reload takes effect
/// without a restart.
pub fn spawn(st: Arc<AppState>) {
    tokio::spawn(async move {
        // A short initial delay lets boot finish writing before the first scan.
        tokio::time::sleep(Duration::from_secs(2)).await;
        loop {
            let sleep_ms = match pass(&st).await {
                Ok(next) => next,
                Err(e) => {
                    // The runner must never die: a broken pass is logged and
                    // retried, because a silent stop is the original bug.
                    tracing::warn!("schedule tick failed: {e:#}");
                    MAX_SLEEP_MS
                }
            };
            tokio::time::sleep(Duration::from_millis(
                sleep_ms.clamp(MIN_SLEEP_MS, MAX_SLEEP_MS) as u64,
            ))
            .await;
        }
    });
}

/// Test seam: run one pass with the SAME code the loop uses (a test that
/// called a reimplementation would prove nothing).
#[doc(hidden)]
pub async fn pass_for_test(st: &Arc<AppState>) -> i64 {
    match pass(st).await {
        Ok(ms) => ms,
        Err(e) => {
            tracing::warn!("schedule test pass failed: {e:#}");
            MAX_SLEEP_MS
        }
    }
}

/// Test seam: make a row due immediately, so a case does not have to sleep for
/// the real interval. Only the DUE TIME moves; every other transition still
/// goes through the production `step`.
#[doc(hidden)]
pub async fn force_due_for_test(st: &Arc<AppState>, id: &str, at: i64) {
    crate::v2::schedule_mark(
        st,
        id,
        serde_json::json!({
            "next_due_ms": at,
            "retry_at": serde_json::Value::Null,
            "time_updated": now_ms(),
        }),
    )
    .ok();
}

/// One pass: fire everything due, return how long to sleep before the next.
async fn pass(st: &Arc<AppState>) -> anyhow::Result<i64> {
    if !crate::v2::schedules_enabled(st) {
        // Nothing is due while the feature is off; the rows keep their due
        // times, so turning it back on catches up like any missed slot.
        return Ok(MAX_SLEEP_MS);
    }
    let now = now_ms();
    let due = crate::v2::schedules_due(st, now)?;
    let mut next_wake = MAX_SLEEP_MS;
    for row in due {
        // A failing row must not stop the others.
        if let Err(e) = step(st, &row, now).await {
            tracing::warn!(
                "schedule {} failed to fire: {e:#}",
                row["id"].as_str().unwrap_or("?")
            );
            let _ = crate::v2::schedule_mark(
                st,
                row["id"].as_str().unwrap_or(""),
                json!({
                    "last_outcome": "failed",
                    "retry_at": now + FAILED_RETRY_MS,
                    "time_updated": now,
                }),
            );
        }
        // Re-read what is left so the sleep covers the NEAREST future slot.
        if let Some(at) = crate::v2::schedules_next_wake(st, now)? {
            next_wake = next_wake.min(at - now);
        }
    }
    Ok(next_wake)
}

async fn step(st: &Arc<AppState>, row: &Value, now: i64) -> anyhow::Result<()> {
    let id = row["id"].as_str().unwrap_or("");
    let sid = row["session_id"].as_str().unwrap_or("");
    let due = match row["next_due_ms"].as_i64() {
        Some(d) => d,
        None => return Ok(()),
    };
    // The session may have been deleted between the query and now (cascade
    // normally removes the row; this covers a concurrent delete).
    if !ocserve_store::session_exists(&st.db, sid)? {
        return Ok(());
    }

    // Previous wake still queued: re-arm the session and skip this slot. A lost
    // wake after admission is recoverable; a second prompt is not.
    if let Some(prev) = row["last_message_id"].as_str()
        && !prev.is_empty()
        && ocserve_store::inbox::get(&st.db, prev)?.is_some()
    {
        st.coordinators
            .wake(sid, ocserve_core::coordinator::WakeScope::Input);
        let next = following(row, now);
        let done = finished(row, next, row["remaining_runs"].as_i64());
        crate::v2::schedule_mark(
            st,
            id,
            json!({
                "last_outcome": "skipped",
                "retry_at": Value::Null,
                "status": if done { "done" } else { "active" },
                "next_due_ms": next,
                "time_updated": now,
            }),
        )?;
        return Ok(());
    }

    let late = now - due >= LATE_AFTER_MS;
    let run = row["run_count"].as_i64().unwrap_or(0) + 1;
    let msg_id = format!("msg_{id}_{run}");
    let prompt = row["prompt"].as_str().unwrap_or("");
    let next = following(row, now);
    let remaining = row["remaining_runs"].as_i64().map(|n| n - 1);
    let done = finished(row, next, remaining);

    // Deterministic ids make the admission idempotent across a restart: the
    // row keeps run_count until AFTER the admit succeeds, so a crash before
    // the update re-admits the same id (INSERT OR IGNORE → no duplicate) and a
    // crash after it never re-admits.
    let payload = json!({
        "text": prompt,
        "files": [],
        "agents": [],
        "skills": [],
        "metadata": {
            "schedule": {
                "id": id,
                "name": row["name"].as_str().filter(|n| !n.is_empty()),
                "run": run,
                "dueAt": due,
                "late": late,
            }
        },
    });
    let (seq, _evt) = ocserve_store::emit_event(
        &st.writer,
        Some(sid),
        "session.inbox.enqueued",
        &json!({"inboxID": msg_id, "sessionID": sid}),
    )?;
    let fresh =
        ocserve_store::inbox::admit(&st.writer, sid, &msg_id, "user", &payload, "steer", seq)?;
    if fresh {
        st.coordinators
            .wake(sid, ocserve_core::coordinator::WakeScope::Steer);
        let st2 = st.clone();
        let sid2 = sid.to_string();
        tokio::spawn(async move {
            crate::v2::drain_session(st2, sid2).await;
        });
        ocserve_metrics::labeled_counter("ocserve_schedule_fired_total", "outcome=\"fired\"", 1);
    } else {
        // Already admitted by an earlier pass: record the run anyway so the
        // next firing does not reuse this message id forever.
        ocserve_metrics::labeled_counter("ocserve_schedule_fired_total", "outcome=\"skipped\"", 1);
    }

    crate::v2::schedule_mark(
        st,
        id,
        json!({
            "run_count": run,
            "last_message_id": msg_id,
            "last_fired_at": now,
            "last_outcome": if late { "late" } else { "fired" },
            "retry_at": Value::Null,
            "remaining_runs": remaining,
            "status": if done { "done" } else { "active" },
            "next_due_ms": if done { Value::Null } else { json!(next) },
            "time_updated": now,
        }),
    )?;
    Ok(())
}

/// The first slot strictly after `now`, or `None` for a one-shot / exhausted
/// row. Mirrors upstream `following()`.
fn following(row: &Value, now: i64) -> Option<i64> {
    // The shared row mapper hands back the PARSED value (the wire shape).
    let when = &row["when_json"];
    match when["type"].as_str() {
        Some("every") => {
            let every = crate::v2::parse_duration_ms(when["every"].as_str().unwrap_or(""))?;
            if every <= 0 {
                return None;
            }
            // Keep the phase and skip missed slots instead of bursting.
            let due = row["next_due_ms"].as_i64().unwrap_or(now);
            Some(due + ((now - due).div_euclid(every) + 1) * every)
        }
        Some("cron") => crate::v2::next_cron_ms(when["cron"].as_str().unwrap_or(""), now),
        // `in` / `at` fire once.
        _ => None,
    }
}

/// Upstream `finished()`: no next slot, no runs left, or past the end bound.
fn finished(row: &Value, next: Option<i64>, remaining: Option<i64>) -> bool {
    let Some(next) = next else { return true };
    if remaining.is_some_and(|r| r <= 0) {
        return true;
    }
    if let Some(end) = row["ends_at_ms"].as_i64()
        && next > end
    {
        return true;
    }
    false
}
