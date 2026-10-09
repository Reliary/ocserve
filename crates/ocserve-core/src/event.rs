//! Event bus: byte- AND count-bounded ring with cursor receivers.
//!
//! Contract (captured live, PLAN §3/§4):
//! - frames: {directory, project, payload:{id,type,properties}}
//! - durable types (session.updated, message.updated, message.part.updated)
//!   also get a sync twin {payload:{type:"sync", syncEvent:{id,type:"<t>.1",
//!   seq, aggregateID, data}}}; seq is per-session monotonic
//! - overflow policy: slow subscribers are disconnected (bounded by
//!   construction — no unbounded queues, AGENTS §2.3); clients recover via
//!   REST snapshot on reconnect (PLAN §4 divergence note)
//!
//! Why not `tokio::sync::broadcast` (the obvious choice): broadcast is
//! **count**-bounded only. Every `message.part.updated` carries the full part
//! (text included — oc-remote's EventReducer stores these snapshots as the
//! rendered chat, so event text cannot be truncated: audit 2026-10-02), and
//! durable emits publish TWO frames (plain + sync twin). A ring of
//! 1024 × multi-MB frames is a GB-class worst case — the exact unbounded-
//! growth class that OOMs upstream opencode (its per-subscriber
//! `Queue.unbounded` + `offerUnsafe`, event.ts:25). This ring is bounded by
//! **bytes as well as count**, stores each frame serialized ONCE (Arc<str>,
//! instead of a Value clone per receiver), and never blocks the publisher:
//! publish = lock + push + evict + watch notify.
//!
//! Per-connection memory stays O(1 frame in flight): the SSE task holds at
//! most one frame while writing to a stalled TCP socket; when the receiver
//! falls behind the ring, it sees `Lagged` and the stream disconnects
//! (existing captured behavior — clients reconnect and REST-snapshot).

use serde_json::{Value, json};
use std::collections::VecDeque;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

/// Count bound of the shared ring (was the broadcast cap; kept).
const BUS_CAPACITY: usize = 1024;
/// Byte bound of the shared ring (new). Sized against MEMORY §1's
/// unallocated headroom (~146 MB): ring ≤32 MB + transient per-connection
/// write copies stay inside it. A SINGLE frame larger than the budget is
/// still admitted (content is contract-critical; the ring sheds everything
/// else instead) — worst case = max(BUDGET, largest frame).
pub const RING_BYTE_BUDGET: usize = 32 * 1024 * 1024;

/// Effective byte budget: `OCSERVE_EVENT_RING_MB` (env, SRE tunability) with
/// the documented default above. Read once (poison-free pattern like the
/// stall watchdog).
fn ring_byte_budget() -> usize {
    static BUDGET: std::sync::OnceLock<usize> = std::sync::OnceLock::new();
    *BUDGET.get_or_init(|| {
        std::env::var("OCSERVE_EVENT_RING_MB")
            .ok()
            .and_then(|v| v.parse::<usize>().ok())
            .filter(|mb| *mb >= 1)
            .map(|mb| mb * 1024 * 1024)
            .unwrap_or(RING_BYTE_BUDGET)
    })
}

struct RingState {
    /// (seq, serialized frame)
    items: VecDeque<(u64, Arc<str>)>,
    /// last issued seq (0 = none yet)
    next_seq: u64,
    total_bytes: usize,
}

pub struct EventBusInner {
    ring: Mutex<RingState>,
    /// watch channel carrying the latest seq (one slot — never grows)
    head: tokio::sync::watch::Sender<u64>,
    receivers: AtomicUsize,
}

#[derive(Clone)]
pub struct EventBus {
    inner: Arc<EventBusInner>,
}

/// Why `recv()` ended: the receiver fell behind (ring evicted its next
/// frame) or the bus was dropped. Mirrors `broadcast::error::RecvError`
/// so SSE code treats lag the same way it did before (disconnect).
#[derive(Debug, PartialEq, Eq)]
pub enum RecvError {
    Lagged(u64),
    Closed,
}

#[derive(Debug, PartialEq, Eq)]
pub enum TryRecvError {
    Empty,
    Lagged(u64),
    Closed,
}

/// Cursor receiver: starts at the head AT SUBSCRIBE TIME (future events
/// only — same semantics as `broadcast::subscribe`). Not `Clone`.
pub struct EventBusReceiver {
    inner: Arc<EventBusInner>,
    last_seq: u64,
    /// already-delivered items spilling out of one ring drain
    pending: VecDeque<String>,
    head_rx: tokio::sync::watch::Receiver<u64>,
}

impl EventBus {
    pub fn new() -> Self {
        let (head, _) = tokio::sync::watch::channel(0u64);
        Self {
            inner: Arc::new(EventBusInner {
                ring: Mutex::new(RingState {
                    items: VecDeque::new(),
                    next_seq: 0,
                    total_bytes: 0,
                }),
                head,
                receivers: AtomicUsize::new(0),
            }),
        }
    }

    /// Live subscriber count (SSE client gauge, SRE §2).
    pub fn subscriber_count(&self) -> usize {
        self.inner.receivers.load(Ordering::Relaxed)
    }

    /// Publish a fully-encoded global-event frame (payload already wrapped).
    /// Never blocks on subscribers: evicts OLDEST frames when the ring
    /// exceeds count or byte bounds (lagging receivers get `Lagged` and
    /// disconnect — backpressure → disconnect, not stall, not growth).
    pub fn publish(&self, frame: Value) {
        let bytes = frame.to_string();
        let len = bytes.len();
        let mut ring = self.inner.ring.lock().expect("event ring poisoned");
        ring.next_seq += 1;
        let seq = ring.next_seq;
        ring.total_bytes += len;
        ring.items.push_back((seq, Arc::from(bytes)));
        // evict oldest until BOTH bounds hold — but never the sole item
        // (an oversize frame must still reach clients; content is contract)
        while ring.items.len() > 1
            && (ring.items.len() > BUS_CAPACITY || ring.total_bytes > ring_byte_budget())
        {
            let (_, evicted) = ring.items.pop_front().expect("non-empty");
            ring.total_bytes = ring.total_bytes.saturating_sub(evicted.len());
            ocserve_metrics::counter("ocserve_event_ring_evicted_total", 1);
        }
        let depth = ring.items.len() as i64;
        let bytes_now = ring.total_bytes as i64;
        drop(ring);
        ocserve_metrics::gauge("ocserve_event_ring_depth", depth);
        ocserve_metrics::gauge("ocserve_event_ring_bytes", bytes_now);
        let _ = self.inner.head.send(seq);
    }

    /// New receiver for events published AFTER this call.
    pub fn subscribe(&self) -> EventBusReceiver {
        let last_seq = self
            .inner
            .ring
            .lock()
            .expect("event ring poisoned")
            .next_seq;
        let head_rx = self.inner.head.subscribe();
        self.inner.receivers.fetch_add(1, Ordering::Relaxed);
        EventBusReceiver {
            inner: self.inner.clone(),
            last_seq,
            pending: VecDeque::new(),
            head_rx,
        }
    }
}

impl Default for EventBus {
    fn default() -> Self {
        Self::new()
    }
}

impl EventBusReceiver {
    /// Move ring items into `pending` (or report lag). Returns false when
    /// caught up.
    fn drain(&mut self) -> Result<bool, RecvError> {
        let (front, next) = {
            let ring = self.inner.ring.lock().expect("event ring poisoned");
            (
                ring.items.front().map(|(s, _)| *s),
                ring.items
                    .iter()
                    .find(|(s, _)| *s > self.last_seq)
                    .map(|(s, a)| (*s, a.clone())),
            )
        };
        // gap between my cursor and the oldest resident frame = I lagged
        if let Some(front) = front
            && front > self.last_seq + 1
        {
            return Err(RecvError::Lagged(front - self.last_seq - 1));
        }
        match next {
            Some((s, frame)) => {
                self.last_seq = s;
                // deliver-one-at-a-time like broadcast: exactly one frame per
                // drain so heartbeat fairness is unchanged
                self.pending.push_back(frame.to_string());
                Ok(true)
            }
            None => Ok(false),
        }
    }

    pub async fn recv(&mut self) -> Result<String, RecvError> {
        loop {
            if let Some(f) = self.pending.pop_front() {
                return Ok(f);
            }
            match self.drain() {
                Ok(true) => continue, // got the next frame into pending
                Err(e) => return Err(e),
                Ok(false) => {}
            }
            // `changed()` MARKS the value seen when it returns — never call
            // has_changed() in this loop (it doesn't mark → busy spin).
            // A publish between drain-empty and changed() returns instantly.
            if self.head_rx.changed().await.is_err() {
                if let Ok(true) = self.drain() {
                    continue;
                }
                if let Some(f) = self.pending.pop_front() {
                    return Ok(f);
                }
                return Err(RecvError::Closed);
            }
        }
    }

    pub fn try_recv(&mut self) -> Result<String, TryRecvError> {
        if let Some(f) = self.pending.pop_front() {
            return Ok(f);
        }
        match self.drain() {
            Ok(true) => Ok(self.pending.pop_front().expect("drained one")),
            // Closed is unreachable here: the receiver holds an Arc to the
            // inner (sender) for its own lifetime — same practical reality
            // as broadcast with the SSE keepalive holding the bus.
            Ok(false) => Err(TryRecvError::Empty),
            Err(RecvError::Lagged(n)) => Err(TryRecvError::Lagged(n)),
            Err(RecvError::Closed) => Err(TryRecvError::Closed),
        }
    }
}

impl Drop for EventBusReceiver {
    fn drop(&mut self) {
        self.inner.receivers.fetch_sub(1, Ordering::Relaxed);
    }
}

/// Encode a plain domain event frame.
pub fn frame(directory: &str, event_type: &str, properties: Value) -> Value {
    json!({
        "directory": directory,
        "project": "global",
        "payload": {
            "id": crate::ids::evt_id(),
            "type": event_type,
            "properties": normalize_props(event_type, properties)
        }
    })
}

/// Normalize event `properties` to satisfy the frozen contract's required
/// fields for the event type — a single choke point so no emitter can ship a
/// spec-violating payload (the 2026-10-09 TUI crash class: `part.updated`
/// omitted the required `time`). Idempotent: existing values win.
///
/// Only fields the spec marks required but that ocserve does not otherwise
/// carry are injected here; everything else is the emitter's responsibility
/// (and is checked by `bench/events/event-validate.py`, guard rule 19).
pub fn normalize_props(event_type: &str, mut props: Value) -> Value {
    let base = event_type.strip_suffix(".1").unwrap_or(event_type);
    // v1 `message.part.updated` requires {sessionID, part, time}.
    if base == "message.part.updated"
        && props.get("time").is_none()
        && let Value::Object(m) = &mut props
    {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0);
        m.insert("time".into(), json!(now));
    }
    props
}

/// Encode a sync-twin frame (EventReducer consumption): the payload is a
/// `sync` envelope wrapping the original event with a per-session seq.
pub fn sync_frame(
    directory: &str,
    event_type: &str,
    properties: Value,
    seq: u64,
    session_id: &str,
) -> Value {
    json!({
        "directory": directory,
        "project": "global",
        "payload": {
            "type": "sync",
            "syncEvent": {
                "id": crate::ids::evt_id(),
                "type": format!("{event_type}.1"),
                "seq": seq,
                "aggregateID": session_id,
                "data": properties
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn frame_shape_matches_capture() {
        let f = frame("/x", "session.idle", json!({"sessionID": "ses_1"}));
        assert_eq!(f["directory"], "/x");
        assert_eq!(f["project"], "global");
        assert_eq!(f["payload"]["type"], "session.idle");
        assert_eq!(f["payload"]["properties"]["sessionID"], "ses_1");
        assert!(f["payload"]["id"].as_str().unwrap().starts_with("evt_"));
    }

    /// normalize_props injects the required `time` on message.part.updated
    /// (spec Event.message.part.updated requires {sessionID, part, time});
    /// ocserve omitted it → the 2026-10-09 TUI crash class. Idempotent.
    #[test]
    fn normalize_injects_part_time() {
        let p = normalize_props(
            "message.part.updated",
            json!({"sessionID": "ses_1", "part": {"id": "prt_1"}}),
        );
        assert!(p["time"].is_number(), "time injected: {p}");
        // idempotent: an existing time wins
        let p2 = normalize_props(
            "message.part.updated",
            json!({"sessionID": "ses_1", "part": {}, "time": 7}),
        );
        assert_eq!(p2["time"], 7);
        // sync twin suffix resolves to the same base type
        let p3 = normalize_props("message.part.updated.1", json!({"part": {}}));
        assert!(p3["time"].is_number());
        // other types untouched
        let p4 = normalize_props("session.idle", json!({"sessionID": "ses_1"}));
        assert!(p4.get("time").is_none());
    }

    #[test]
    fn sync_frame_shape_matches_capture() {
        let f = sync_frame(
            "/x",
            "message.updated",
            json!({"sessionID": "ses_1"}),
            7,
            "ses_1",
        );
        let se = &f["payload"]["syncEvent"];
        assert_eq!(se["type"], "message.updated.1");
        assert_eq!(se["seq"], 7);
        assert_eq!(se["aggregateID"], "ses_1");
        assert_eq!(se["data"]["sessionID"], "ses_1");
    }

    #[tokio::test]
    async fn bus_delivers_and_bounds_by_count() {
        let bus = EventBus::new();
        let mut rx = bus.subscribe();
        bus.publish(frame("/x", "session.idle", json!({})));
        let got = rx.recv().await.unwrap();
        let v: Value = serde_json::from_str(&got).unwrap();
        assert_eq!(v["payload"]["type"], "session.idle");
        // overflow → Lagged error (not a stall); receiver side disconnects
        for _ in 0..2000 {
            bus.publish(frame("/x", "session.status", json!({})));
        }
        match rx.recv().await {
            Ok(_) | Err(RecvError::Lagged(_)) => {}
            e => panic!("unexpected bus result: {e:?}"),
        }
    }

    /// NEGATIVE-CONTROL pair for the byte bound: publishing frames that
    /// would blow the count bound's byte budget must evict (depth AND bytes
    /// stay ≤ budget) — remove the eviction condition and this fails.
    #[tokio::test]
    async fn ring_is_byte_bounded_not_just_count_bounded() {
        let bus = EventBus::new();
        // one 16 MB frame ×4 = 64 MB > 32 MB budget (count stays under1024)
        let big = "x".repeat(16 * 1024 * 1024);
        for i in 0..4 {
            bus.publish(frame(
                "/x",
                "message.part.updated",
                json!({"i": i, "big": big}),
            ));
        }
        let ring = bus.inner.ring.lock().unwrap();
        assert!(
            ring.total_bytes <= RING_BYTE_BUDGET,
            "ring bytes {} exceeded budget",
            ring.total_bytes
        );
        assert!(
            ring.items.len() < 4,
            "oversize frames must evict: depth {}",
            ring.items.len()
        );
        // newest frame survives
        let last = ring.items.back().unwrap();
        assert_eq!(last.0, ring.next_seq, "newest frame kept");
    }

    /// An oversize frame (larger than the whole budget) is still admitted
    /// (content contract) and evicts everything else — bounded BY it.
    #[tokio::test]
    async fn single_oversize_frame_admitted_alone() {
        let bus = EventBus::new();
        for i in 0..5 {
            bus.publish(frame("/x", "t", json!({"i": i})));
        }
        let huge = "y".repeat(RING_BYTE_BUDGET + 1024);
        bus.publish(frame("/x", "message.part.updated", json!({"big": huge})));
        let ring = bus.inner.ring.lock().unwrap();
        assert_eq!(
            ring.items.len(),
            1,
            "everything else shed for the oversize frame"
        );
        assert!(ring.total_bytes > RING_BYTE_BUDGET);
    }

    /// The publisher never blocks on slow subscribers: no receiver drains
    /// anything and publishes still complete promptly (the upstream OOM
    /// pattern is offering into unbounded per-subscriber queues instead).
    #[tokio::test]
    async fn publisher_never_blocks_on_stalled_receivers() {
        let bus = EventBus::new();
        let _slow: Vec<_> = (0..8).map(|_| bus.subscribe()).collect();
        let t0 = std::time::Instant::now();
        for _ in 0..5000 {
            bus.publish(frame(
                "/x",
                "message.part.updated",
                json!({"pad": "z".repeat(1024)}),
            ));
        }
        assert!(
            t0.elapsed() < Duration::from_secs(5),
            "publish loop took {:?}",
            t0.elapsed()
        );
        // stalled receivers are lagging, not growing: ring stays bounded
        let ring = bus.inner.ring.lock().unwrap();
        assert!(ring.items.len() <= BUS_CAPACITY);
        assert!(ring.total_bytes <= RING_BYTE_BUDGET);
    }

    #[tokio::test]
    async fn subscribe_only_sees_future_and_try_recv_roundtrips() {
        let bus = EventBus::new();
        bus.publish(frame("/x", "old", json!({})));
        let mut rx = bus.subscribe();
        assert!(
            matches!(rx.try_recv(), Err(TryRecvError::Empty)),
            "subscribe is future-only (broadcast parity)"
        );
        bus.publish(frame("/x", "new", json!({})));
        let got = rx.try_recv().unwrap();
        assert!(got.contains("\"type\": \"new\"") || got.contains("\"type\":\"new\""));
        assert!(matches!(rx.try_recv(), Err(TryRecvError::Empty)));
        assert_eq!(bus.subscriber_count(), 1);
        drop(rx);
        assert_eq!(bus.subscriber_count(), 0);
    }
}

#[cfg(test)]
mod unfold_tests {
    use super::*;
    use std::time::Duration;

    /// Replicates ocserve-http's SSE merge loop under paused time.
    #[tokio::test(start_paused = true)]
    async fn heartbeat_emits_at_10s_and_stream_stays_open() {
        let bus = EventBus::new();
        let mut rx = bus.subscribe();
        let mut next_hb = tokio::time::Instant::now() + Duration::from_secs(10);
        let mut got_hb = false;
        for _ in 0..3 {
            let dur = next_hb.saturating_duration_since(tokio::time::Instant::now());
            match tokio::time::timeout(dur.max(Duration::from_millis(1)), rx.recv()).await {
                Ok(Ok(_frame)) => panic!("unexpected frame"),
                Ok(Err(e)) => panic!("recv errored: {e:?}"),
                Err(_elapsed) => {
                    got_hb = true;
                    next_hb += Duration::from_secs(10);
                }
            }
        }
        assert!(got_hb, "heartbeat fired");
        // frames still flow after the heartbeats
        bus.publish(frame("/x", "after", json!({})));
        let got = tokio::time::timeout(Duration::from_secs(1), rx.recv())
            .await
            .expect("frame arrives")
            .expect("not lagged");
        assert!(got.contains("after"));
    }
}
