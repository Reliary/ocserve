//! refine-core: session domain — event bus, IDs, sync prompt runner.
//!
//! Wire-invisible internals adopt v2 engineering where the plan says so
//! (PLAN §11): durable event seq, bounded ring, crash-recoverable claims
//! land with M2b/M3 milestones.

pub mod event;
pub mod ids;
pub mod prompt;

pub use event::EventBus;
