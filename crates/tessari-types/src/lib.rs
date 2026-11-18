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
// `expect_used` and `as_conversions` govern production code; a test states its own expectations.
#![cfg_attr(test, allow(clippy::expect_used, clippy::as_conversions))]

mod acknowledge;
mod analyzer;
mod assertion;
mod calendar;
mod condition;
mod conflict_policy;
pub mod containment;
mod edits;
mod feed_image;
mod field_kind;
mod float_order;
mod geojson;
mod geometry;
mod identity_kind;
mod ids;
pub mod json;
mod nesting;
mod number;
mod path;
mod reach;
mod record_id;
mod refusal_class;
mod replication;
mod replication_class;
mod stemmer;
mod text;
mod time;
mod value;
mod write_kind;

pub use acknowledge::{Acknowledge, Acknowledgement};
pub use analyzer::{Analyzer, Filter, Language, Memo, TOKENIZER_GENERATION, Token};
pub use assertion::{Assertion, Operand};
pub use calendar::Civil;
pub use condition::{BinaryOp, apply};
pub use conflict_policy::ConflictPolicy;
pub use edits::within as within_edits;
pub use feed_image::FeedImage;
pub use field_kind::FieldKind;
pub use geojson::{Malformed, from_geojson, geojson_name, to_geojson};
pub use geometry::{Geometry, Polygon, Position, Ring};
pub use identity_kind::IdentityKind;
pub use ids::{
    DatabaseId, EdgeKindId, Epoch, FieldId, GraphId, IndexId, NamespaceId, Sequence, ShardId,
    TableId,
};
pub use nesting::MAX_NESTING;
pub use number::Number;
pub use path::{Path, Step};
pub use reach::Reach;
pub use record_id::RecordId;
pub use refusal_class::RefusalClass;
pub use replication::Replication;
pub use replication_class::ReplicationClass;
pub use stemmer::stem;
pub use text::{article, parse_uuid, string_to_literal, uuid_to_text};
pub use time::{Datetime, Duration};
pub use value::{RecordRef, Value, ValueRange};
pub use write_kind::WriteKind;

/// The split bound the store bakes into a bare `SPLIT AUTOMATICALLY` and an
/// inherited namespace default: a shard holding more records than this is
/// split. The single home for the number, so the parser that fills a bare
/// declaration and the balancer that resolves an inherited one cannot drift
/// apart.
pub const BARE_SPLIT_ABOVE_RECORDS: u32 = 1_000_000;
/// The merge bound paired with [`BARE_SPLIT_ABOVE_RECORDS`]: neighbours under
/// this together merge — a fifth of the split bound, the pairing the
/// declare-time oscillation guard accepts with a 2.5× margin.
pub const BARE_SPLIT_MERGE_RECORDS: u32 = 200_000;
