//! Shared value types for TessariDB.
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
mod assertion;
mod calendar;
mod condition;
mod edits;
mod field_kind;
mod geojson;
mod geometry;
mod identity_kind;
mod ids;
mod number;
mod path;
mod record_id;
mod stemmer;
mod text;
mod time;
mod value;

pub use analyzer::{Analyzer, Filter, Token};
pub use assertion::{Assertion, Operand};
pub use calendar::Civil;
pub use condition::{BinaryOp, apply};
pub use edits::within as within_edits;
pub use field_kind::FieldKind;
pub use geojson::{Malformed, from_geojson, geojson_name, to_geojson};
pub use geometry::{Geometry, Polygon, Position, Ring};
pub use identity_kind::IdentityKind;
pub use ids::{DatabaseId, EdgeKindId, FieldId, GraphId, IndexId, NamespaceId, Sequence, TableId};
pub use number::Number;
pub use path::{Path, Step};
pub use record_id::RecordId;
pub use stemmer::stem;
pub use text::{parse_uuid, string_to_literal, uuid_to_text};
pub use time::{Datetime, Duration};
pub use value::{RecordRef, Value, ValueRange};
