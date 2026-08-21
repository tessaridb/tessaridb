//! Shared value types for `bgv-db`.
//!
//! This crate is a dependency leaf: it defines the identifiers and value types
//! every other crate in the workspace speaks in, and depends on nothing inside
//! the workspace itself.
//!
//! What lives here is what more than one layer needs to name — the tenancy
//! identifiers, the log sequence, and the identity of a record. Anything a
//! single layer owns stays in that layer.

#![forbid(unsafe_code)]

mod ids;
mod record_id;

pub use ids::{DatabaseId, NamespaceId, Sequence, TableId};
pub use record_id::RecordId;
