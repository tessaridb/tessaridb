//! The value a record holds.
//!
//! Seventeen types.
//!
//! Fifteen were fixed by the milestone-1 scope; `Geometry` and `Regex` were
//! among three deferred and are now here.
//!
//! The third deferral — a **file reference** — was withdrawn rather than
//! implemented, because it already exists under another name. A bucket is a
//! table whose records are files (`docs/bgvql.md` §6a), so pointing a record at
//! a file is an ordinary [`Value::Record`]: `FETCH` follows it, `RELATE` makes
//! an edge of it, and a grant on the bucket governs metadata and bytes together
//! because there is one table to grant on. A separate `File` type would be a
//! second spelling for all of that, and every consumer would have to handle
//! both.
//!
//! # Absent and null are different values
//!
//! `None` means the field is not there. `Null` means it is there and holds
//! nothing. Collapsing them is the single most common shortcut in a value
//! system and it costs the ability to say "this was never set" as distinct from
//! "this was set to nothing" — a distinction a memory store built on this
//! cannot do without, because "we looked and found nothing" and "we never
//! looked" are different facts.
//!
//! # Ordering is total, and across types it is by declared rank
//!
//! Two values of different types have to compare to something the moment an
//! index holds a column with more than one type in it, and "unspecified" is not
//! an answer a range scan can use. Types therefore carry a rank, values compare
//! within a rank by their own order, and the rank order is part of the contract
//! rather than an accident of how the variants happen to be written down.
//!
//! Numbers are the exception that proves the rule: all three numeric kinds share
//! one rank and compare *semantically*, so `1`, `1.0` and `1.00` are one value.
//! Ranking them apart would make `1 < 1.5` false, which is the kind of wrongness
//! that no test notices until a query returns the wrong rows.

use core::fmt;
use std::collections::{BTreeMap, BTreeSet};
use std::ops::Bound;

use crate::geometry::Geometry;
use crate::ids::TableId;
use crate::number::Number;
use crate::record_id::RecordId;
use crate::time::{Datetime, Duration};

/// A reference to one record, as a value.
///
/// Table and identity together, because an identity alone does not name a
/// record — two tables may each hold `1`.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct RecordRef {
    /// The table the record lives in.
    pub table: TableId,
    /// The record's identity within it.
    pub id: RecordId,
}

impl RecordRef {
    /// Refer to a record.
    #[must_use]
    pub const fn new(table: TableId, id: RecordId) -> Self {
        Self { table, id }
    }
}

impl fmt::Display for RecordRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}", self.table, self.id)
    }
}

/// A span between two values, either end open or closed.
///
/// Its reason to be a value rather than a query construct is that a record
/// identity may itself be a range, which is what makes "every record between
/// these two" expressible without a scan being written out by hand.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ValueRange {
    /// The lower end.
    pub start: Bound<Value>,
    /// The upper end.
    pub end: Bound<Value>,
}

impl ValueRange {
    /// Build a span from its two ends.
    #[must_use]
    pub const fn new(start: Bound<Value>, end: Bound<Value>) -> Self {
        Self { start, end }
    }
}

/// An endpoint reduced to something comparable.
///
/// An open end sorts below any closed one, and at the same value a closed end
/// sorts below an excluded one. The library's own bound type carries no order,
/// which is the right default — "is this range before that one" has more than
/// one sensible answer — so the answer this store uses is written out here.
///
/// It is a **stable total order for storing and indexing ranges**, and it is not
/// a containment or overlap relation. Asking whether one range is inside another
/// is a different question with a different answer.
fn endpoint(bound: &Bound<Value>) -> (u8, Option<&Value>) {
    match bound {
        Bound::Unbounded => (0, None),
        Bound::Included(value) => (1, Some(value)),
        Bound::Excluded(value) => (2, Some(value)),
    }
}

impl PartialOrd for ValueRange {
    fn partial_cmp(&self, other: &Self) -> Option<core::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for ValueRange {
    fn cmp(&self, other: &Self) -> core::cmp::Ordering {
        endpoint(&self.start)
            .cmp(&endpoint(&other.start))
            .then_with(|| endpoint(&self.end).cmp(&endpoint(&other.end)))
    }
}

/// A value in the store.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Value {
    /// The field is not present.
    None,
    /// The field is present and holds nothing.
    Null,
    /// A boolean.
    Bool(bool),
    /// A number — integer, float or exact decimal.
    Number(Number),
    /// Text.
    String(String),
    /// Opaque bytes.
    Bytes(Vec<u8>),
    /// A span of time, which may be negative.
    Duration(Duration),
    /// A point in time.
    Datetime(Datetime),
    /// A universally unique identifier, as its sixteen bytes.
    Uuid([u8; 16]),
    /// A reference to a table.
    Table(TableId),
    /// A reference to one record.
    Record(RecordRef),
    /// An ordered sequence.
    Array(Vec<Value>),
    /// A map from field name to value, kept in name order so that two objects
    /// with the same content encode to the same bytes.
    Object(BTreeMap<String, Value>),
    /// A span between two values.
    ///
    /// Boxed because a range holds values and a value may be a range; without
    /// the box the type would have no finite size.
    Range(Box<ValueRange>),
    /// A collection with no duplicates and no significant order.
    Set(BTreeSet<Value>),
    /// A shape on the sphere.
    ///
    /// The type stores and returns a shape. Answering `INSIDE`, `INTERSECTS` or
    /// a distance over one is an **engine**, and a separate thing: a stored
    /// geometry with no index is already useful, and conflating the two is how
    /// the work stops being schedulable.
    Geometry(Geometry),
    /// A pattern, as its source text.
    ///
    /// Held rather than executed. This store has no regular-expression engine,
    /// so a pattern here round-trips as a pattern — distinguishable from a
    /// string that happens to spell one — and matching is a later capability
    /// rather than an implied one. Saying so is the difference between a type
    /// that is honest about its scope and a type that looks like a feature.
    Regex(String),
}

/// Where each type sits when values of different types are compared.
///
/// The order is declared here, in one place, and is part of the value system's
/// contract. Reordering it reorders every index that holds a mixed column, so it
/// is a data migration and not a refactor.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Rank {
    None,
    Null,
    Bool,
    /// All three numeric kinds share this rank so they compare semantically.
    Number,
    String,
    Bytes,
    Duration,
    Datetime,
    Uuid,
    Table,
    Record,
    Array,
    Object,
    Range,
    Set,
    // Appended, never inserted. The doc comment above says why: a rank's
    // position is where a value of that type sorts among values of every other
    // type, so moving one reorders every index holding a mixed column.
    Geometry,
    Regex,
}

impl Value {
    /// The rank this value's type carries.
    const fn rank(&self) -> Rank {
        match self {
            Self::None => Rank::None,
            Self::Null => Rank::Null,
            Self::Bool(_) => Rank::Bool,
            Self::Number(_) => Rank::Number,
            Self::String(_) => Rank::String,
            Self::Bytes(_) => Rank::Bytes,
            Self::Duration(_) => Rank::Duration,
            Self::Datetime(_) => Rank::Datetime,
            Self::Uuid(_) => Rank::Uuid,
            Self::Table(_) => Rank::Table,
            Self::Record(_) => Rank::Record,
            Self::Array(_) => Rank::Array,
            Self::Object(_) => Rank::Object,
            Self::Range(_) => Rank::Range,
            Self::Set(_) => Rank::Set,
            Self::Geometry(_) => Rank::Geometry,
            Self::Regex(_) => Rank::Regex,
        }
    }

    /// The name of this value's type, as the query language spells it.
    #[must_use]
    pub const fn type_name(&self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Null => "null",
            Self::Bool(_) => "bool",
            Self::Number(_) => "number",
            Self::String(_) => "string",
            Self::Bytes(_) => "bytes",
            Self::Duration(_) => "duration",
            Self::Datetime(_) => "datetime",
            Self::Uuid(_) => "uuid",
            Self::Table(_) => "table",
            Self::Record(_) => "record",
            Self::Array(_) => "array",
            Self::Object(_) => "object",
            Self::Range(_) => "range",
            Self::Set(_) => "set",
            Self::Geometry(_) => "geometry",
            Self::Regex(_) => "regex",
        }
    }

    /// Whether the value is present at all.
    ///
    /// `Null` **is** present: it is a value that says nothing, not the absence
    /// of one.
    #[must_use]
    pub const fn is_present(&self) -> bool {
        !matches!(self, Self::None)
    }
}

impl PartialOrd for Value {
    fn partial_cmp(&self, other: &Self) -> Option<core::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Value {
    fn cmp(&self, other: &Self) -> core::cmp::Ordering {
        let ranks = self.rank().cmp(&other.rank());
        if ranks != core::cmp::Ordering::Equal {
            return ranks;
        }
        match (self, other) {
            (Self::Bool(left), Self::Bool(right)) => left.cmp(right),
            (Self::Number(left), Self::Number(right)) => left.cmp(right),
            (Self::String(left), Self::String(right)) => left.cmp(right),
            (Self::Bytes(left), Self::Bytes(right)) => left.cmp(right),
            (Self::Duration(left), Self::Duration(right)) => left.cmp(right),
            (Self::Datetime(left), Self::Datetime(right)) => left.cmp(right),
            (Self::Uuid(left), Self::Uuid(right)) => left.cmp(right),
            (Self::Table(left), Self::Table(right)) => left.cmp(right),
            (Self::Record(left), Self::Record(right)) => left.cmp(right),
            (Self::Array(left), Self::Array(right)) => left.cmp(right),
            (Self::Object(left), Self::Object(right)) => left.cmp(right),
            (Self::Range(left), Self::Range(right)) => left.cmp(right),
            (Self::Set(left), Self::Set(right)) => left.cmp(right),
            // Both of these carry a payload, so both must be compared here. The
            // wildcard below answers `Equal`, which for a payload-carrying type
            // means a set keeps one of two distinct values and an `ORDER BY`
            // does nothing — silently, which is why the arms are not optional.
            (Self::Geometry(left), Self::Geometry(right)) => left.cmp(right),
            (Self::Regex(left), Self::Regex(right)) => left.cmp(right),
            // Equal ranks and no pair above means both sides are one of the two
            // types that carry no payload, and those have one value each.
            _ => core::cmp::Ordering::Equal,
        }
    }
}

impl From<bool> for Value {
    fn from(value: bool) -> Self {
        Self::Bool(value)
    }
}

impl From<i64> for Value {
    fn from(value: i64) -> Self {
        Self::Number(Number::Integer(value))
    }
}

impl From<f64> for Value {
    fn from(value: f64) -> Self {
        Self::Number(Number::float(value))
    }
}

impl From<String> for Value {
    fn from(value: String) -> Self {
        Self::String(value)
    }
}

impl From<&str> for Value {
    fn from(value: &str) -> Self {
        Self::String(value.to_owned())
    }
}

impl fmt::Display for Value {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::None => f.write_str("NONE"),
            Self::Null => f.write_str("NULL"),
            Self::Bool(value) => write!(f, "{value}"),
            Self::Number(value) => write!(f, "{value}"),
            Self::String(value) => write!(f, "{value:?}"),
            Self::Bytes(value) => write!(f, "<{} bytes>", value.len()),
            Self::Duration(value) => write!(f, "{value}"),
            Self::Datetime(value) => write!(f, "{value}"),
            Self::Uuid(value) => {
                f.write_str("uuid:")?;
                for byte in value {
                    write!(f, "{byte:02x}")?;
                }
                Ok(())
            }
            Self::Table(value) => write!(f, "table:{value}"),
            Self::Record(value) => write!(f, "{value}"),
            Self::Array(values) => write!(f, "<array of {}>", values.len()),
            Self::Object(fields) => write!(f, "<object of {}>", fields.len()),
            Self::Range(_) => f.write_str("<range>"),
            Self::Set(values) => write!(f, "<set of {}>", values.len()),
            Self::Geometry(shape) => {
                write!(f, "<{} of {}>", shape.kind_name(), shape.positions().len())
            }
            // The pattern is shown, and quoted so it is legible as data rather
            // than mistaken for something this store is about to run.
            Self::Regex(pattern) => write!(f, "regex:{pattern:?}"),
        }
    }
}
