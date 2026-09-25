//! The key-value verbs a cache needs (G035).
//!
//! A space is a table whose records hold one value (ADR-0010); this module adds
//! what an application needs to use one instead of a separate cache server —
//! an expiry per key, and (in the files beside this one as they arrive) atomic
//! writes and a key walk. One file per concern, so `execute.rs` only delegates.
//!
//! Everything here works through [`tessari_storage::Transaction`] and nothing
//! below it, so it behaves the same on the memory backend and the disk one.

mod atomic;
mod expiry;
mod walk;

pub(crate) use atomic::{CONFLICT_DEADLINE, retried_on_conflict};
pub(crate) use walk::Walk;
