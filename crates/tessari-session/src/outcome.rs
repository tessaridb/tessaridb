//! What a statement answers with.
//!
//! Four shapes, because the language has four shapes of answer and collapsing
//! them into one would make every caller ask what it got back. A statement that
//! answers nothing says so rather than returning an empty list, which would be
//! indistinguishable from a read that found nothing.

use tessari_types::{RecordId, Value};

/// The result of one statement.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum Outcome {
    /// The statement did its work and has nothing to report.
    Done,
    /// Records, in key order, each with its identity, and how they were found.
    ///
    /// The path is reported rather than inferred because it is the difference
    /// between a read that stays fast as the table grows and one that does not.
    /// A caller that never sees it cannot tell them apart until it is slow.
    Records {
        /// What was found.
        records: Vec<(RecordId, Value)>,
        /// How.
        path: AccessPath,
    },
    /// One value — or [`Value::None`] when the key holds nothing.
    ///
    /// `None` and a stored `Null` are different answers, which is the point of
    /// the value system keeping both.
    Value(Value),
    /// Keys, in order.
    Keys(Vec<RecordId>),
    /// How many records a conditional delete removed.
    ///
    /// A count rather than [`Outcome::Done`], because the whole point of a
    /// retention statement is how much it took: "removed 12 043 readings" is an
    /// operator checking their policy did what they meant, and `done` is that
    /// operator running a `SELECT count(*)` before and after to find out.
    Removed {
        /// How many.
        count: u64,
    },
}

/// How a read reached its records.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AccessPath {
    /// Straight to one record by its identity.
    Record,
    /// Through an index.
    Index,
    /// Through an index read in the order the statement asked for, stopping at
    /// its bound.
    ///
    /// Its own path rather than [`Self::Index`]: this one is chosen by the
    /// `ORDER BY` and the `LIMIT` rather than by a condition, and it is the only
    /// one that falls back — an index that cannot fill the bound leaves the
    /// answer needing records it does not hold, and reports the scan that then
    /// ran.
    Ordered,
    /// Every record of the table was read and tested.
    ///
    /// Correct, and linear in the size of the table. A text search reports this
    /// until a text index exists to serve it; the statement does not change when
    /// one does.
    Scan,
}

impl AccessPath {
    /// A short stable name, for logs and for a client that shows the cost.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Record => "record",
            Self::Index => "index",
            Self::Ordered => "ordered",
            Self::Scan => "scan",
        }
    }
}

impl Outcome {
    /// The records this outcome carries, if it carries any.
    #[must_use]
    pub fn records(&self) -> Option<&[(RecordId, Value)]> {
        match self {
            Self::Records { records, .. } => Some(records),
            _ => None,
        }
    }

    /// How the records were found, if this outcome carries records.
    #[must_use]
    pub const fn path(&self) -> Option<AccessPath> {
        match self {
            Self::Records { path, .. } => Some(*path),
            _ => None,
        }
    }

    /// The single value this outcome carries, if it carries one.
    #[must_use]
    pub const fn value(&self) -> Option<&Value> {
        match self {
            Self::Value(value) => Some(value),
            _ => None,
        }
    }

    /// The keys this outcome carries, if it carries any.
    #[must_use]
    pub fn keys(&self) -> Option<&[RecordId]> {
        match self {
            Self::Keys(keys) => Some(keys),
            _ => None,
        }
    }
}
