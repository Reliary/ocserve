//! `/goal` loop driver — the idle hook half of a Rust port of upstream's v2
//! built-in `opencode.goal` plugin (`packages/core/src/plugin/goal.ts`).
//!
//! Upstream hooks `experimental.idle`, which the runner fires when a session
//! would go idle with nothing pending. ocserve has no such event; the
//! equivalent point is `wake_inbox_after_turn`, called once a prompt turn
//! finishes and the inbox is drained. This module is that hook.
//!
//! The loop is deliberately NOT a detached worker: each continuation is a
//! normal prompt turn, so the goal cannot outlive the session's locks, cannot
//! run concurrently with a user prompt, and is cancelled by the same abort
//! machinery as any other turn.
//!
//! Budget accounting: the reviewer call is TRANSIENT (`RunOpts::transient`),
//! which upstream's `session.generate` also is. Whether upstream bills those
//! tokens to the session meter is the open question recorded as
//! `D-GOAL-REVIEWER-ACCOUNTING`; until it is settled we do NOT add reviewer
//! usage to the session row (a transient call never touches it), and the
//! divergence row says so. The spend is not invisible, though: the reviewer
//! sets `RunOpts::attribution_session`, so its tokens land in
//! `ocserve_transient_tokens_total{owner="session",session="…"}`.
//!
//! Every reviewer call has a wall-clock budget (`OCSERVE_GOAL_REVIEW_SECS`,
//! default 300 s); on expiry the goal pauses rather than holding the wake gate.
//!
//! Concurrency is bounded twice: one driver per session (replace-on-arm) and
//! at most `MAX_CONCURRENT_DRIVERS` across the process; a goal that cannot get
//! a slot is PAUSED with a reason instead of sitting ACTIVE with no driver.
//! Every driver removes its own registry entry through a Drop guard, so an
//! early return cannot leave a finished handle behind.

use crate::{ApiError, AppState, HttpError};
use axum::http::StatusCode;
use ocserve_core::goal::{
    MAX_STALLS, Status, VerdictKind, continuation, is_stalled, judge_prompt, kickoff, parse_verdict,
};
use serde_json::{Value, json};
use std::sync::Arc;

/// This process's boot id. A goal armed by an earlier process comes back
/// paused rather than resuming on its own (upstream `boot()`).
pub fn boot_id() -> &'static str {
    static BOOT: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    BOOT.get_or_init(|| {
        // No rand dependency here: pid + start time is unique per process and
        // readable without a new crate. Upstream uses a UUID; the value is
        // only ever compared for equality.
        let t = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        format!("boot-{}-{t}", std::process::id())
    })
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Pause every ACTIVE goal whose boot id is not ours — goals left by an
/// earlier process (upstream's boot scan). Called once at serve start.
pub async fn pause_goals_from_earlier_boots(st: &Arc<AppState>) {
    let db = st.db.clone();
    let rows = match crate::run_blocking(&db, ocserve_store::active_goals).await {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!("goal boot scan failed: {e:#}");
            return;
        }
    };
    let ours = boot_id();
    for (sid, mut row) in rows {
        if row.boot == ours {
            continue;
        }
        row.status = Status::Paused.as_str().to_string();
        row.reason = Some("The server restarted while the goal was active".to_string());
        row.updated_at = now_ms();
        let w = st.writer.clone();
        let sid2 = sid.clone();
        let r = row.clone();
        if let Err(e) =
            crate::run_blocking(&db, move |_| ocserve_store::set_goal(&w, &sid2, Some(&r))).await
        {
            // A swallowed write leaves the goal ACTIVE with no driver, which
            // is the exact state the boot scan exists to prevent.
            tracing::error!("goal boot-pause persist failed session={sid}: {e:#}");
            continue;
        }
        tracing::info!("goal paused at boot session={sid}");
    }
}

/// The idle hook. Called after a turn finishes (see `wake_inbox_after_turn`).
///
/// Returns true when it started a continuation turn, so the caller knows the
/// session is busy again (and must not be considered idle).
pub async fn after_turn(st: &Arc<AppState>, sid: &str) -> bool {
    let db = st.db.clone();
    let sid_owned = sid.to_string();
    let goal =
        match crate::run_blocking(&db, move |db| ocserve_store::get_goal(db, &sid_owned)).await {
            Ok(Some(g)) => g,
            Ok(None) => return false,
            Err(e) => {
                tracing::warn!("goal read failed session={sid}: {e:#}");
                return false;
            }
        };
    if goal.status != Status::Active.as_str() {
        return false;
    }
    let mut goal = goal;
    // A goal armed by another process must not drive this one.
    if goal.boot != boot_id() {
        settle(
            st,
            sid,
            &mut goal,
            Status::Paused,
            "The server restarted while the goal was active",
        )
        .await;
        return false;
    }
    if goal.rounds >= goal.max_rounds {
        let msg = format!("Reached the limit of {} rounds", goal.max_rounds);
        settle(st, sid, &mut goal, Status::Paused, &msg).await;
        return false;
    }
    // A running shell on THIS session is background work the goal may be
    // waiting on; its completion re-enters this hook.
    //
    // HONEST LIMIT (D-GOAL-NO-BACKGROUND-SHELLS): the registry now carries an
    // owning session, but no serving route populates it — `POST /api/shell`
    // has no session, and `/api/session/{id}/shell` runs the command as a
    // tool-less turn rather than registering a shell. So this list is empty in
    // practice and a `wait` verdict is never legitimately produced. The filter
    // is kept (and unit-tested against an attributed entry) so the mechanism
    // starts working the day a route attributes its spawns; the previous
    // comment claimed the registry already did, which was false.
    let shells: Vec<String> = st
        .shells
        .list()
        .into_iter()
        .filter(|s| s["sessionID"].as_str() == Some(sid))
        .filter(|s| s["status"].as_str() == Some("running"))
        .filter_map(|s| s["command"].as_str().map(str::to_string))
        .collect();
    let verdict = match review(st, sid, &goal.objective, &shells).await {
        Some(v) => v,
        None => {
            settle(
                st,
                sid,
                &mut goal,
                Status::Paused,
                "The goal reviewer gave no usable verdict",
            )
            .await;
            return false;
        }
    };
    // `/goal pause`, `clear` or a new goal may have landed while the reviewer
    // ran: re-read and refuse to apply a stale decision (upstream `replace`).
    let db = st.db.clone();
    let sid_owned = sid.to_string();
    let latest =
        match crate::run_blocking(&db, move |db| ocserve_store::get_goal(db, &sid_owned)).await {
            Ok(Some(g)) => g,
            _ => return false,
        };
    if latest.id != goal.id
        || latest.status != Status::Active.as_str()
        || latest.objective != goal.objective
    {
        return false;
    }
    let mut latest = latest;
    match verdict.verdict {
        // Unreachable by construction: `review` rejects `wait` when no shell
        // is running, and this server's shell registry carries no session id,
        // so the judge is always told "No background shells are running"
        // (D-GOAL-NO-BACKGROUND-SHELLS). Kept for parity with the verdict
        // union; it must not leave the goal active.
        VerdictKind::Wait => false,
        VerdictKind::Done => {
            settle(st, sid, &mut latest, Status::Complete, &verdict.reason).await;
            false
        }
        VerdictKind::Blocked | VerdictKind::Paused => {
            settle(st, sid, &mut latest, Status::Paused, &verdict.reason).await;
            false
        }
        VerdictKind::Continue => {
            let used_tools = used_tools_since(st, sid, &latest.id).await;
            let stalled = is_stalled(latest.reason.as_deref(), &verdict.reason, used_tools);
            let stalls = if stalled {
                latest.stalls.unwrap_or(0) + 1
            } else {
                0
            };
            if stalls >= MAX_STALLS {
                settle(
                    st,
                    sid,
                    &mut latest,
                    Status::Paused,
                    &format!("Not making progress: {}", verdict.reason),
                )
                .await;
                return false;
            }
            latest.rounds += 1;
            latest.stalls = Some(stalls);
            latest.reason = Some(verdict.reason.clone());
            latest.updated_at = now_ms();
            let w = st.writer.clone();
            let row = latest.clone();
            let sid_owned = sid.to_string();
            if let Err(e) = crate::run_blocking(&db, move |_| {
                ocserve_store::set_goal(&w, &sid_owned, Some(&row))
            })
            .await
            {
                // A swallowed write here silently disables the 50-round cap
                // (rounds never advances) — the guard's "never swallow a
                // storage write" rule, which its `*.write(` match missed.
                tracing::error!("goal round persist failed session={sid}: {e:#}");
                ocserve_metrics::labeled_counter(
                    "ocserve_goal_total",
                    "event=\"persist_failed\"",
                    1,
                );
            }
            let text = continuation(&latest.objective, &verdict);
            spawn_driver(st, sid, text, latest.id.clone(), true);
            true
        }
    }
}

async fn settle(
    st: &Arc<AppState>,
    sid: &str,
    goal: &mut ocserve_store::GoalRow,
    status: Status,
    reason: &str,
) {
    goal.status = status.as_str().to_string();
    goal.reason = Some(reason.to_string());
    goal.updated_at = now_ms();
    let db = st.db.clone();
    let w = st.writer.clone();
    let row = goal.clone();
    let sid_owned = sid.to_string();
    if let Err(e) = crate::run_blocking(&db, move |_| {
        ocserve_store::set_goal(&w, &sid_owned, Some(&row))
    })
    .await
    {
        // A failed settle is how "paused" becomes a lie: the row stays ACTIVE
        // and, with no driver, nothing will ever move it (SRE review).
        tracing::error!("goal settle persist failed session={sid}: {e:#}");
        ocserve_metrics::labeled_counter("ocserve_goal_total", "event=\"persist_failed\"", 1);
    }
    tracing::info!("goal {} session={sid}: {reason}", status.as_str());
    ocserve_metrics::labeled_counter(
        "ocserve_goal_total",
        &format!("event=\"{}\"", status.as_str()),
        1,
    );
    announce(st, sid, &goal.to_json());
}

/// Whether the model called any tool since the goal's last kickoff or
/// continuation. Without that message in view (for example after a
/// compaction) the round counts as progress, exactly as upstream does.
async fn used_tools_since(st: &Arc<AppState>, sid: &str, goal_id: &str) -> bool {
    let db = st.db.clone();
    let sid_owned = sid.to_string();
    let goal_id = goal_id.to_string();
    crate::run_blocking(&db, move |db| {
        // Bounded tail: the kickoff/continuation is within the last few
        // messages in every real case, and when it is not, the fallback
        // below counts the round as progress (upstream semantics).
        let msgs = ocserve_store::tail_messages(db, &sid_owned, 200)?;
        // The goal tag lives on the PART (`TextPart.metadata`, sdk 1.19.5);
        // `UserMessage` has no metadata field at all.
        let start = msgs.iter().rposition(|(info, parts)| {
            let role = info["role"].as_str().unwrap_or("");
            role == "user"
                && parts.iter().any(|p| {
                    p["metadata"]["source"].as_str() == Some("goal")
                        && p["metadata"]["goalID"].as_str() == Some(goal_id.as_str())
                })
        });
        let Some(start) = start else { return Ok(true) };
        Ok(msgs[start + 1..].iter().any(|(info, parts)| {
            info["role"].as_str() == Some("assistant")
                && parts.iter().any(|p| p["type"].as_str() == Some("tool"))
        }))
    })
    .await
    .unwrap_or(true)
}

/// The reviewer call: a TRANSIENT generation (no session writes), exactly the
/// machinery `/api/experimental/generate` uses. Returns None when the reviewer
/// produced no parsable verdict.
/// Wall-clock budget for one reviewer call. Generous (a real model call, and
/// the goal is unattended) but finite: without it a chatty provider holds the
/// per-session wake gate forever.
fn review_budget() -> std::time::Duration {
    let secs: u64 = std::env::var("OCSERVE_GOAL_REVIEW_SECS")
        .ok()
        .and_then(|v| v.parse().ok())
        .filter(|s| *s >= 30)
        .unwrap_or(300);
    std::time::Duration::from_secs(secs)
}

async fn review(
    st: &Arc<AppState>,
    sid: &str,
    objective: &str,
    shells: &[String],
) -> Option<ocserve_core::goal::Verdict> {
    let prompt = judge_prompt(objective, shells);
    let req = ocserve_core::wire::PromptRequest {
        message_id: None,
        model: None,
        parts: vec![json!({"type": "text", "text": prompt})],
        ..Default::default()
    };
    // The reviewer runs against the SESSION's model, so a session pinned to a
    // provider keeps judging with the same provider. `build_prompt_context`
    // resolves session model → default; the transient id never touches the DB.
    let ctx = match crate::build_prompt_context(st, &req, sid) {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!("goal reviewer context failed session={sid}: {}", e.message);
            return None;
        }
    };
    let writer = st.writer.clone();
    // THE SESSION'S OWN ID. The judge must read the conversation it is
    // judging ("using only evidence in the conversation above", judge.txt);
    // history is loaded by session id, so a synthetic id made every review
    // run against an EMPTY conversation and the model could only guess.
    // Upstream's `ctx.session.generate({sessionID})` runs against the real
    // session for the same reason. `persist_user:false` + `transient` keep
    // the judge turn out of history and off every session-scoped emission.
    let res = tokio::time::timeout(
        review_budget(),
        ocserve_core::prompt::run_prompt_with(
            &ctx,
            &writer,
            sid,
            &req,
            ocserve_core::prompt::RunOpts {
                persist_user: false,
                skip_history: false,
                tools_enabled: false,
                auto_title: false,
                transient: true,
                // The reviewer is a real model call for THIS session: attribute
                // its tokens to telemetry (it still writes nothing).
                attribution_session: Some(sid.to_string()),
                ..Default::default()
            },
        ),
    )
    .await;
    let res = match res {
        Ok(r) => r,
        Err(_) => {
            // The stream watchdog bounds each read, not the whole call. A
            // provider that dribbles events kept the wake gate closed, so the
            // goal sat ACTIVE with no pending review (2026-11-05 SRE review).
            tracing::warn!(
                "goal reviewer exceeded its {}s budget session={sid}",
                review_budget().as_secs()
            );
            ocserve_metrics::labeled_counter("ocserve_goal_total", "event=\"review_timeout\"", 1);
            return None;
        }
    };
    let (_info, parts) = match res {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!("goal reviewer failed session={sid}: {e:#}");
            return None;
        }
    };
    let text = parts
        .iter()
        .find(|p| p["type"] == "text")
        .and_then(|p| p["text"].as_str())
        .unwrap_or("");
    let v = parse_verdict(text);
    // "wait" is only offered while a shell runs; without one nothing would
    // wake the session to judge again (upstream enforces the same rule).
    if shells.is_empty() && v.as_ref().is_some_and(|v| v.verdict == VerdictKind::Wait) {
        return None;
    }
    v
}

/// The kickoff/continuation part: a synthetic text part carrying the goal
/// tag. `TextPart.metadata` is where the tag lives (a `UserMessage` has no
/// metadata field), and `synthetic` is what makes a timeline render it as a
/// notice row instead of something the user typed.
/// The goal tag rides on the PART (`TextPart.metadata`).
///
/// ACCEPTED-LOW: a client can forge `metadata:{source:"goal"}` on an inbound
/// part, which only affects its OWN session's stall accounting (whether a tool
/// ran since the anchor). There is no cross-session effect and no privilege
/// change, so it is documented rather than blocked (blocking would also block
/// the legitimate continuation, which uses the same shape).
pub(crate) fn goal_part(text: &str, goal_id: &str, synthetic: bool) -> Value {
    json!({
        "type": "text",
        "text": text,
        "synthetic": synthetic,
        "metadata": {"source": "goal", "goalID": goal_id},
    })
}

/// Result of arming a goal, before any turn runs.
pub(crate) enum Arm {
    /// A control word ran (pause/resume/clear): the new state, or Null for
    /// clear. No turn is started for these.
    Control(Value),
    /// A new objective was persisted; the caller decides how to deliver the
    /// kickoff (v2 spawns, v1 runs it inside the command request).
    Armed {
        row: ocserve_store::GoalRow,
        text: String,
    },
}

/// Run one goal turn IN THE CALLER'S REQUEST and hand back its
/// `(assistant info, parts)`.
///
/// The v1 command route must answer `SessionV1.WithParts`, so it cannot spawn
/// the kickoff and reply with a goal-state object (the pre-fix behaviour: the
/// response's `info` was a Goal.State, not an AssistantMessage, so any strict
/// client parse failed, and the parts array was empty). The turn is the same
/// one the spawned path runs; only the joiner differs.
///
/// The caller must NOT already hold the session lock (this acquires it).
pub(crate) async fn run_turn_now(
    st: &Arc<AppState>,
    sid: &str,
    text: &str,
    goal_id: &str,
    extra_parts: Vec<Value>,
    model: Option<ocserve_core::wire::ModelRef>,
) -> anyhow::Result<(Value, Vec<Value>)> {
    let release = match crate::lock_session(st, sid).await {
        Ok(r) => r,
        Err(e) => {
            // The state was already persisted by `arm`. Without a turn the
            // goal would sit ACTIVE with no driver (2026-11-05 review), so a
            // refusal pauses it rather than pretending it is running.
            pause_goal(st, sid, &format!("The goal could not start: {}", e.message)).await;
            return Err(anyhow::anyhow!("{}", e.message));
        }
    };
    let guard = release.arc().lock().await;
    {
        let _admission = st.v2_session_admission_gate.read().await;
        if let Err(e) = crate::check_v1_session_admission(st, sid).await {
            drop(guard);
            return Err(anyhow::anyhow!("{}", e.message));
        }
    }
    // With attachments the kickoff rides as the user's own message (upstream's
    // `attached` branch); a bare kickoff is a synthetic notice.
    let attached = !extra_parts.is_empty();
    let mut parts = vec![goal_part(text, goal_id, !attached)];
    parts.extend(extra_parts);
    let req = ocserve_core::wire::PromptRequest {
        message_id: None,
        // The v1 command payload carries the client's model selection; the
        // kickoff must honor it or a command sent with `model` runs on a
        // different provider than the button the user pressed.
        model,
        parts,
        ..Default::default()
    };
    let ctx = match crate::build_prompt_context(st, &req, sid) {
        Ok(c) => c,
        Err(e) => {
            drop(guard);
            pause_goal(st, sid, &format!("The goal could not start: {}", e.message)).await;
            return Err(anyhow::anyhow!("{}", e.message));
        }
    };
    let writer = st.writer.clone();
    let out = ocserve_core::prompt::run_prompt_with(
        &ctx,
        &writer,
        sid,
        &req,
        ocserve_core::prompt::RunOpts {
            auto_title: false,
            ..Default::default()
        },
    )
    .await;
    drop(guard);
    match out {
        Ok((info, parts)) => {
            // Judge again (this is the loop), but do NOT await it here: the
            // reviewer is a real model call, and the v1 command response must
            // not wait on a second completion. Upstream's command returns once
            // the synthetic kickoff is delivered, for the same reason.
            spawn_wake_public(st, sid);
            Ok((info, parts))
        }
        Err(e) => {
            // A failed kickoff must not leave an active goal that can never
            // make progress; upstream pauses on a session error.
            if let Ok(Some(mut goal)) = ocserve_store::get_goal(&st.db, sid)
                && goal.status == Status::Active.as_str()
            {
                settle(
                    st,
                    sid,
                    &mut goal,
                    Status::Paused,
                    &format!("The goal stopped after an error: {e:#}"),
                )
                .await;
            }
            spawn_wake_public(st, sid);
            Err(e)
        }
    }
}

/// Fire the goal idle hook on the runtime without making the caller wait for
/// the reviewer completion.
///
/// This MUST call `after_turn` directly. An earlier version called
/// `wake_inbox_after_turn`, which calls this function again; the dedup guard
/// then suppressed the inner call and the reviewer never ran (caught by the
/// goal e2e on the first run).
pub(crate) fn spawn_wake_public(st: &Arc<AppState>, sid: &str) {
    // One pending review per session. Overlapping wakes (a user turn and a
    // goal turn finishing together) would both call the reviewer and both
    // spawn a continuation, doubling rounds and cost (2026-11-05 review).
    {
        let mut w = st.goal_wakes.lock();
        if w.contains(sid) {
            return;
        }
        w.insert(sid.to_string());
    }
    let st2 = st.clone();
    let sid2 = sid.to_string();
    tokio::spawn(async move {
        after_turn(&st2, &sid2).await;
        st2.goal_wakes.lock().remove(&sid2);
    });
}

/// A short assistant notice for a control word (`pause`/`resume`/`clear`).
///
/// The command route must return `WithParts`; persisting a real assistant
/// message keeps that contract even though no model ran. Returns the GOAL
/// STATE alongside it under `goal`, so a client that wants the state can read
/// it without an RPC round-trip.
pub(crate) async fn goal_control_notice(
    st: &Arc<AppState>,
    sid: &str,
    which: &str,
) -> Result<Value, HttpError> {
    let state = match ocserve_store::get_goal(&st.db, sid) {
        Ok(v) => v.map(|g| g.to_json()).unwrap_or(Value::Null),
        Err(_) => Value::Null,
    };
    let notice = match which {
        "pause" => "Goal paused.",
        "resume" => "Goal resumed.",
        "clear" => "Goal cleared.",
        _ => "Goal updated.",
    };
    let text = match &state {
        Value::Null => notice.to_string(),
        s => format!("{notice}\n{}", s["objective"].as_str().unwrap_or("")),
    };
    let now = now_ms();
    // Attribute the notice to the session's own model so a client that shows
    // a model badge does not render an empty provider (no model call runs).
    let (pid, mid) = crate::resolve_model(st, None, sid);
    let info = json!({
        "id": ocserve_core::ids::msg_id(),
        "sessionID": sid,
        "role": "assistant",
        "mode": "build",
        "agent": "build",
        "path": {"cwd": st.paths["directory"].clone(), "root": "/"},
        "cost": 0.0,
        "tokens": {"total": 0, "input": 0, "output": 0, "reasoning": 0,
                   "cache": {"write": 0, "read": 0}},
        "modelID": mid,
        "providerID": pid,
        "time": {"created": now, "completed": now},
        "finish": "stop",
    });
    let part = json!({
        "type": "text",
        "id": ocserve_core::ids::prt_id(),
        "sessionID": sid,
        "messageID": info["id"],
        "text": text,
        "synthetic": true,
    });
    let writer = st.writer.clone();
    ocserve_store::insert_message(
        &writer,
        Some(&*st.blobs),
        sid,
        &info,
        std::slice::from_ref(&part),
    )
    .map_err(|e| {
        HttpError::from(ApiError {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            name: "InternalError",
            message: format!("{e:#}"),
        })
    })?;
    crate::emit_durable_http(
        st,
        sid,
        "message.updated",
        json!({"sessionID": sid, "info": info.clone()}),
    )
    .map_err(HttpError::from)?;
    crate::emit_durable_http(
        st,
        sid,
        "message.part.updated",
        json!({"sessionID": sid, "part": part.clone()}),
    )
    .map_err(HttpError::from)?;
    Ok(json!({"info": info, "parts": [part], "goal": state}))
}

/// Spawn a goal continuation turn, TRACKED so `/abort`, session delete and
/// `/goal pause|clear` actually stop it.
///
/// Before every turn the driver re-reads the goal and refuses to run unless it
/// is still ACTIVE and still the same goal id. Without the status check a
/// `/goal pause` sent during a reviewer call still ran the queued continuation
/// to completion (2026-11-05 review); without the registry entry, `/abort`
/// cancelled only the current prompt task, not the loop that respawns it.
/// Removes this driver's registry entry when the task ends for ANY reason
/// (success, early return, panic during unwind). Identity is the token, so a
/// replacement driver spawned meanwhile is never removed by the outgoing one.
struct DriverSlot {
    st: Arc<AppState>,
    sid: String,
    token: u64,
}

impl Drop for DriverSlot {
    fn drop(&mut self) {
        let mut m = self.st.goal_tasks.lock();
        if m.get(&self.sid).is_some_and(|(t, _)| *t == self.token) {
            m.remove(&self.sid);
        }
    }
}

/// Cap on goal loops driving AT ONCE across sessions. Each may run up to
/// `max_rounds` model turns, so the per-session cap alone bounds one session
/// and not the process (review L6).
const MAX_CONCURRENT_DRIVERS: usize = 4;

fn spawn_driver(st: &Arc<AppState>, sid: &str, text: String, goal_id: String, synthetic: bool) {
    static NEXT_TOKEN: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
    let token = NEXT_TOKEN.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    // One driver per session: replace (and cancel) a stale one.
    {
        let mut tasks = st.goal_tasks.lock();
        if let Some((_, old)) = tasks.remove(sid) {
            old.abort();
        }
        // Global bound: a session already running is replaced above, so this
        // only refuses when OTHER sessions fill the slots. Refusing is honest
        // (the goal stays ACTIVE and the next idle wake retries), and it keeps
        // N armed sessions from multiplying model spend.
        if tasks.len() >= MAX_CONCURRENT_DRIVERS {
            tracing::warn!(
                "goal: {MAX_CONCURRENT_DRIVERS} loops already driving; deferring session={sid}"
            );
            drop(tasks);
            // Pause rather than leave an ACTIVE goal nothing will drive: a
            // deferred goal the operator can see and resume beats one that
            // quietly never runs.
            let st2 = st.clone();
            let sid2 = sid.to_string();
            tokio::spawn(async move {
                pause_goal(
                    &st2,
                    &sid2,
                    &format!("Deferred: {MAX_CONCURRENT_DRIVERS} goals are already running"),
                )
                .await;
            });
            return;
        }
    }
    let st2 = st.clone();
    let sid2 = sid.to_string();
    let slot = DriverSlot {
        st: st2.clone(),
        sid: sid2.clone(),
        token,
    };
    let goal_for_id = goal_id.clone();
    let handle = tokio::spawn(async move {
        // Dropped on every exit path, including the early returns below.
        let _slot = slot;
        let goal_id = goal_for_id;
        // Re-check right before the turn: the goal may have been paused,
        // cleared or replaced while this task waited for the lock.
        let current = crate::run_blocking(&st2.db, {
            let db = st2.db.clone();
            let sid3 = sid2.clone();
            move |_| ocserve_store::get_goal(&db, &sid3)
        })
        .await
        .ok()
        .flatten();
        let still_active = current.as_ref().is_some_and(|g| {
            g.id == goal_id && g.status == Status::Active.as_str() && g.boot == boot_id()
        });
        if !still_active {
            return;
        }
        let release = match crate::lock_session(&st2, &sid2).await {
            Ok(r) => r,
            Err(e) => {
                // Persisted state says ACTIVE but no turn can run: pause
                // rather than leaving a goal that looks alive and is not.
                pause_goal(
                    &st2,
                    &sid2,
                    &format!("The goal could not start: {}", e.message),
                )
                .await;
                return;
            }
        };
        let guard = release.arc().lock().await;
        // Second check under the lock: a pause that landed between the read
        // and the acquire must still win.
        let current = crate::run_blocking(&st2.db, {
            let db = st2.db.clone();
            let sid3 = sid2.clone();
            move |_| ocserve_store::get_goal(&db, &sid3)
        })
        .await
        .ok()
        .flatten();
        let still_active = current.as_ref().is_some_and(|g| {
            g.id == goal_id && g.status == Status::Active.as_str() && g.boot == boot_id()
        });
        if !still_active {
            drop(guard);
            return;
        }
        let req = ocserve_core::wire::PromptRequest {
            message_id: None,
            model: None,
            parts: vec![goal_part(&text, &goal_id, synthetic)],
            ..Default::default()
        };
        let ctx = match crate::build_prompt_context(&st2, &req, &sid2) {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!("goal turn context failed session={sid2}: {}", e.message);
                drop(guard);
                pause_goal(
                    &st2,
                    &sid2,
                    &format!("The goal could not start: {}", e.message),
                )
                .await;
                return;
            }
        };
        let writer = st2.writer.clone();
        let turn = ocserve_core::prompt::run_prompt_with(
            &ctx,
            &writer,
            &sid2,
            &req,
            ocserve_core::prompt::RunOpts {
                auto_title: false,
                ..Default::default()
            },
        )
        .await;
        drop(guard);
        if let Err(e) = turn {
            // A failing continuation must PAUSE, not retry. Without this the
            // loop re-reviews, is told "continue", fails the same way, and
            // burns a provider call per cycle forever (2026-11-05 review:
            // only `run_turn_now` handled this, and only for the kickoff).
            tracing::error!("goal turn failed session={sid2}: {e:#}");
            ocserve_metrics::labeled_counter("ocserve_goal_total", "event=\"turn_failed\"", 1);
            // SURFACE the failure: this path calls `run_prompt_with` directly,
            // so `run_prompt`'s wrapper (durable session.error + [turn
            // stopped] part + finalize + idle) never ran and a client that saw
            // `busy` would spin forever (2026-11-05 SRE review). Same shape the
            // wrapper emits, so the client sees one kind of error.
            crate::emit_session_error_event(
                &st2,
                &sid2,
                &format!("The goal stopped after an error: {e:#}"),
            );
            let db = st2.db.clone();
            let sid3 = sid2.clone();
            if let Ok(Some(mut g)) = crate::run_blocking(&db, {
                let db = st2.db.clone();
                let sid4 = sid3.clone();
                move |_| ocserve_store::get_goal(&db, &sid4)
            })
            .await
                && g.id == goal_id
                && g.status == Status::Active.as_str()
            {
                settle(
                    &st2,
                    &sid3,
                    &mut g,
                    Status::Paused,
                    &format!("The goal stopped after an error: {e:#}"),
                )
                .await;
            }
            return;
        }
        // The turn finished: judge again (this is the loop). Only if the goal
        // is still active; a paused/cleared goal must not be resurrected.
        let current = crate::run_blocking(&st2.db, {
            let db = st2.db.clone();
            let sid3 = sid2.clone();
            move |_| ocserve_store::get_goal(&db, &sid3)
        })
        .await
        .ok()
        .flatten();
        let still_active = current.as_ref().is_some_and(|g| {
            g.id == goal_id && g.status == Status::Active.as_str() && g.boot == boot_id()
        });
        if still_active {
            crate::wake_inbox_after_turn(&st2, &sid2).await;
        }
    });
    st.goal_tasks
        .lock()
        .insert(sid.to_string(), (token, handle));
}

/// Public pause used by the abort/interrupt paths (upstream pauses an active
/// goal on an interrupted execution that the USER caused).
pub(crate) async fn pause_if_active(st: &Arc<AppState>, sid: &str, reason: &str) {
    pause_goal(st, sid, reason).await;
}

/// Pause the goal if it is still active, with `reason`. Used on the paths
/// where a kickoff was requested but no turn could run.
async fn pause_goal(st: &Arc<AppState>, sid: &str, reason: &str) {
    let db = st.db.clone();
    let sid_owned = sid.to_string();
    if let Ok(Some(mut g)) =
        crate::run_blocking(&db, move |db| ocserve_store::get_goal(db, &sid_owned)).await
        && g.status == Status::Active.as_str()
    {
        settle(st, sid, &mut g, Status::Paused, reason).await;
    }
}

/// Cancel a session's goal driver (abort / delete / pause / clear).
pub fn cancel_driver(st: &Arc<AppState>, sid: &str) {
    if let Some((_, h)) = st.goal_tasks.lock().remove(sid) {
        h.abort();
    }
}

/// Start `/goal <objective>`: persist the state and send the kickoff prompt
/// the same way the command surface would. Used by BOTH the v1 command route
/// and the v2 command admission, so the two surfaces cannot drift.
/// Validate the argument and persist the state, WITHOUT starting a turn.
///
/// Split out of `start_goal` because the two surfaces deliver the kickoff
/// differently: the v2 command route admits and returns 204, so it spawns the
/// turn; the v1 command route must answer `SessionV1.WithParts`, so it runs
/// the kickoff inside the request and returns the turn's real assistant
/// message. Both must share the state machine (and the control words).
pub(crate) async fn arm(st: &Arc<AppState>, sid: &str, objective: &str) -> Result<Arm, String> {
    let objective = ocserve_core::goal::unquote(objective);
    if objective.is_empty() {
        return Err(usage(None));
    }
    let lower = objective.to_lowercase();
    if matches!(lower.as_str(), "pause" | "resume" | "clear") {
        return control(st, sid, &lower).await.map(Arm::Control);
    }
    if objective.chars().count() > ocserve_core::goal::MAX_OBJECTIVE {
        return Err(format!(
            "A goal objective is limited to {} characters (got {}).",
            ocserve_core::goal::MAX_OBJECTIVE,
            objective.chars().count()
        ));
    }
    let now = now_ms();
    let row = ocserve_store::GoalRow {
        // Monotonic suffix: two arms in the same millisecond must not share
        // an id, or a stale reviewer applies to the new objective.
        id: ocserve_core::goal::mint_id(),
        objective: objective.clone(),
        status: Status::Active.as_str().to_string(),
        reason: None,
        rounds: 0,
        max_rounds: ocserve_core::goal::MAX_ROUNDS,
        stalls: None,
        boot: boot_id().to_string(),
        updated_at: now,
    };
    let db = st.db.clone();
    let w = st.writer.clone();
    let row2 = row.clone();
    let sid_owned = sid.to_string();
    crate::run_blocking(&db, move |_| {
        ocserve_store::set_goal(&w, &sid_owned, Some(&row2))
    })
    .await
    .map_err(|e| format!("{e:#}"))?;
    // Titles come from the first user message and a notice kickoff is not
    // one, so name the session here when it still has the fallback title.
    let title = ocserve_core::goal::truncate(&objective, 60);
    // Upstream renames only when the session still carries the fallback
    // title (a user-named session keeps its name).
    let db = st.db.clone();
    let sid_owned = sid.to_string();
    let is_fallback = crate::run_blocking(&db, move |db| {
        ocserve_store::load_session_wire(db, &sid_owned)
    })
    .await
    .ok()
    .flatten()
    .map(|s| {
        let t = s["title"].as_str().unwrap_or("");
        t.is_empty() || t.starts_with("New session - ")
    })
    .unwrap_or(false);
    if is_fallback {
        let db = st.db.clone();
        let sid_owned = sid.to_string();
        let w = st.writer.clone();
        let t = title.clone();
        let _ = crate::run_blocking(&db, move |_| {
            w.write(vec![ocserve_store::writer::WriteOp::Sql {
                sql: "UPDATE session SET title = ?2 WHERE id = ?1 AND (title = '' OR title LIKE 'New session - %')".into(),
                params: vec![serde_json::json!(sid_owned), serde_json::json!(t)],
            }])
        })
        .await;
    }
    ocserve_metrics::labeled_counter("ocserve_goal_total", "event=\"armed\"", 1);
    let state = row.to_json();
    announce(st, sid, &state);
    Ok(Arm::Armed {
        row,
        text: kickoff(&objective),
    })
}

/// v2 command path: persist + spawn the kickoff, returning the goal state.
/// Upstream's command `execute` is asynchronous (`session.synthetic` wakes
/// the loop) and the HTTP caller already has its 204, so this must not block.
/// Publish a goal state change on the live bus. Uses `session.updated` with
/// an additive `goal` member rather than a bespoke event type: the frozen
/// Event union has no `session.goal.*` variant, and an unknown type is
/// rejected by the nightly event validator (and ignored by every client).
pub(crate) fn announce(st: &Arc<AppState>, sid: &str, goal: &Value) {
    let dir = st.paths["directory"].as_str().unwrap_or("/");
    st.bus.publish(ocserve_core::event::frame(
        dir,
        "session.updated",
        json!({"sessionID": sid, "goal": goal}),
    ));
}

pub async fn start_goal(
    st: &Arc<AppState>,
    sid: &str,
    objective: &str,
    attached: bool,
) -> Result<serde_json::Value, String> {
    match arm(st, sid, objective).await? {
        Arm::Control(v) => Ok(v),
        Arm::Armed { row, text } => {
            // A prompt can carry attachments; a synthetic notice cannot. With
            // attachments the kickoff goes through as the user's own message
            // (upstream's `attached` branch).
            continue_with_text(st, sid, &text, &row.id, attached);
            Ok(row.to_json())
        }
    }
}

async fn control(st: &Arc<AppState>, sid: &str, which: &str) -> Result<serde_json::Value, String> {
    let db = st.db.clone();
    let sid_owned = sid.to_string();
    let goal = crate::run_blocking(&db, move |db| ocserve_store::get_goal(db, &sid_owned))
        .await
        .map_err(|e| format!("{e:#}"))?;
    let Some(mut goal) = goal else {
        return Err(match which {
            "clear" => "No goal is set.".to_string(),
            "pause" => "No active goal to pause.".to_string(),
            _ => "No paused goal to resume.".to_string(),
        });
    };
    let now = now_ms();
    match which {
        "pause" => {
            if goal.status != Status::Active.as_str() {
                return Err("No active goal to pause.".to_string());
            }
            cancel_driver(st, sid);
            goal.status = Status::Paused.as_str().to_string();
            goal.reason = Some("Paused by the user".to_string());
            goal.updated_at = now;
            let w = st.writer.clone();
            let row = goal.clone();
            let sid_owned = sid.to_string();
            if let Err(e) = crate::run_blocking(&db, move |_| {
                ocserve_store::set_goal(&w, &sid_owned, Some(&row))
            })
            .await
            {
                return Err(format!("could not pause the goal: {e:#}"));
            }
            Ok(goal.to_json())
        }
        "resume" => {
            if goal.status != Status::Paused.as_str() {
                return Err("No paused goal to resume.".to_string());
            }
            goal.status = Status::Active.as_str().to_string();
            goal.reason = None;
            goal.stalls = None;
            goal.rounds = 1; // the resume continuation is round 1 of a new budget
            goal.boot = boot_id().to_string();
            goal.updated_at = now;
            let w = st.writer.clone();
            let row = goal.clone();
            let sid_owned = sid.to_string();
            if let Err(e) = crate::run_blocking(&db, move |_| {
                ocserve_store::set_goal(&w, &sid_owned, Some(&row))
            })
            .await
            {
                return Err(format!("could not resume the goal: {e:#}"));
            }
            let text = continuation(
                &goal.objective,
                &ocserve_core::goal::Verdict {
                    verdict: VerdictKind::Continue,
                    reason: "The user resumed the goal.".to_string(),
                    next_step: None,
                },
            );
            continue_with_text(st, sid, &text, &goal.id, false);
            Ok(goal.to_json())
        }
        _ => {
            cancel_driver(st, sid);
            let w = st.writer.clone();
            let sid_owned = sid.to_string();
            crate::run_blocking(&db, move |_| ocserve_store::set_goal(&w, &sid_owned, None))
                .await
                .map_err(|e| format!("could not clear the goal: {e:#}"))?;
            Ok(serde_json::Value::Null)
        }
    }
}

fn usage(goal: Option<&ocserve_store::GoalRow>) -> String {
    let u = "Usage: /goal <objective> | pause | resume | clear";
    match goal {
        Some(g) => {
            let st = if g.status == "active" {
                "Goal active"
            } else if g.status == "paused" {
                "Goal paused"
            } else {
                "Goal achieved"
            };
            let mut s = format!("{st}: {}", g.objective);
            if let Some(r) = &g.reason {
                s.push('\n');
                s.push_str(r);
            }
            s.push('\n');
            s.push_str(u);
            s
        }
        None => u.to_string(),
    }
}

/// Kickoff/resume delivery: synthetic by default (the timeline shows one
/// notice row), or a real user message when the command carried attachments.
fn continue_with_text(st: &Arc<AppState>, sid: &str, text: &str, goal_id: &str, as_user: bool) {
    // Upstream's `attached` branch runs a real prompt; both are the same turn
    // here except for the `synthetic` flag (a notice vs the user's message).
    spawn_driver(st, sid, text.to_string(), goal_id.to_string(), !as_user);
}
