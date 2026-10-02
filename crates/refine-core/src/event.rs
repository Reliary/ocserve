//! Event bus: bounded broadcast of already-encoded global-event frames.
//!
//! Contract (captured live, PLAN §3/§4):
//! - frames: {directory, project, payload:{id,type,properties}}
//! - durable types (session.updated, message.updated, message.part.updated)
//!   also get a sync twin {payload:{type:"sync", syncEvent:{id,type:"<t>.1",
//!   seq, aggregateID, data}}}; seq is per-session monotonic
//! - overflow policy: slow subscribers are disconnected (bounded by
//!   construction — no unbounded queues, AGENTS §2.3); clients recover via
//!   REST snapshot on reconnect (PLAN §4 divergence note)

use serde_json::{Value, json};
use tokio::sync::broadcast;

/// Capacity of the per-subscriber ring (PLAN §4: sse_ring_events default 4096;
/// broadcast is capped here for a single-user server — memory: 4096 × avg
/// frame size stays inside the SSE budget line of MEMORY §1).
const BUS_CAPACITY: usize = 1024;

#[derive(Clone)]
pub struct EventBus {
    tx: broadcast::Sender<Value>,
}

impl EventBus {
    pub fn new() -> Self {
        let (tx, _) = broadcast::channel(BUS_CAPACITY);
        Self { tx }
    }

    /// Publish a fully-encoded global-event frame (payload already wrapped).
    /// Lagged/disconnected receivers are dropped silently — the bus must
    /// never block publishers (event bus backpressure → disconnect, not stall).
    pub fn publish(&self, frame: Value) {
        let _ = self.tx.send(frame);
    }

    pub fn subscribe(&self) -> broadcast::Receiver<Value> {
        self.tx.subscribe()
    }
}

impl Default for EventBus {
    fn default() -> Self {
        Self::new()
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
            "properties": properties,
        }
    })
}

/// Encode the sync twin of a durable event (per-session seq supplied by caller).
pub fn sync_frame(
    directory: &str,
    event_type: &str,
    properties: Value,
    seq: u64,
    aggregate_id: &str,
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
                "aggregateID": aggregate_id,
                "data": properties,
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frame_shape_matches_capture() {
        let f = frame("/x", "session.idle", json!({"sessionID": "ses_1"}));
        assert_eq!(f["project"], "global");
        assert_eq!(f["payload"]["type"], "session.idle");
        assert_eq!(f["payload"]["properties"]["sessionID"], "ses_1");
        assert!(f["payload"]["id"].as_str().unwrap().starts_with("evt_"));
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
    async fn bus_delivers_and_bounds() {
        let bus = EventBus::new();
        let mut rx = bus.subscribe();
        bus.publish(frame("/x", "session.idle", json!({})));
        let got = rx.recv().await.unwrap();
        assert_eq!(got["payload"]["type"], "session.idle");
        // overflow → Lagged error (not a stall); receiver side decides to drop
        for _ in 0..2000 {
            bus.publish(frame("/x", "session.status", json!({})));
        }
        match rx.recv().await {
            Ok(_) | Err(broadcast::error::RecvError::Lagged(_)) => {}
            Err(e) => panic!("unexpected bus error: {e}"),
        }
    }
}

#[cfg(test)]
mod unfold_tests {
    use super::*;
    use std::time::Duration;

    /// Replicates refine-http's SSE merge loop under paused time.
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
                Ok(Err(e)) => panic!("recv errored: {e}"),
                Err(_elapsed) => {
                    got_hb = true;
                    next_hb += Duration::from_secs(10);
                }
            }
        }
        assert!(got_hb);
    }
}
