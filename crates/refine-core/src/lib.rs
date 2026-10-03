//! refine-core: session domain — event bus, IDs, permission rendezvous,
//! sync prompt runner with the tool loop.
//!
//! Wire-invisible internals adopt v2 engineering where the plan says so
//! (PLAN §11): durable event seq, bounded ring, crash-recoverable claims
//! land with M3 milestones.

pub mod compact;
pub mod compaction;
pub mod event;
pub mod ids;
pub mod permission;
pub mod prompt;
pub mod question;

pub use event::EventBus;
pub use permission::PermissionGate;
