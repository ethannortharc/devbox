//! Observability — the glass box (§7).
//!
//! The agent inside a box captures kernel-level activity and streams it here;
//! this module receives it, stores it, and answers questions about it.
//!
//! - [`event`] — the canonical schema (§11.1), mirrored in `agent/event`.
//! - [`store`] — the per-box SQLite store and its query API (§7.4).
//! - [`collector`] — the socket listener and the agent handshake (§11.3).
//! - [`correlate`] — the join that turns a wall of events into a story (§7.2).

pub mod collector;
pub mod correlate;
pub mod event;
pub mod store;

pub use event::{Event, EventType};
pub use store::{Query, Retention, Store};
