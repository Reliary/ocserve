//! refine-store: SQLite (pragmas/schema/writer) + chunked blob store.
//!
//! Invariants (see STORAGE.md / MEMORY.md / AGENTS.md):
//! - PRAGMAs only via `pragma::{create_new,open_writer,open_reader}`
//! - no connection held across `.await` (enforced by the scoped `Writer` API)
//! - payloads via `blob::BlobStore`, never as SQLite row payloads > metadata

pub mod blob;
pub mod pragma;
pub mod schema;
pub mod writer;

pub use blob::BlobStore;
pub use writer::{WriteOp, Writer};
