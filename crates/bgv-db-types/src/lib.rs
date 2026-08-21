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

mod analyzer;
mod field_kind;
mod ids;
mod number;
mod path;
mod record_id;
mod text;
mod time;
mod value;

pub use analyzer::{Analyzer, Filter};
pub use field_kind::FieldKind;
pub use ids::{DatabaseId, FieldId, IndexId, NamespaceId, Sequence, TableId};
pub use number::Number;
pub use path::{Path, Step};
pub use record_id::RecordId;
pub use text::parse_uuid;
pub use time::{Datetime, Duration};
pub use value::{RecordRef, Value, ValueRange};
