//! What a statement answers with.
//!
//! Four shapes, because the language has four shapes of answer and collapsing
//! them into one would make every caller ask what it got back. A statement that
//! answers nothing says so rather than returning an empty list, which would be
//! indistinguishable from a read that found nothing.

use bgv_db_types::{RecordId, Value};

/// The result of one statement.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum Outcome {
    /// The statement did its work and has nothing to report.
    Done,
    /// Records, in key order, each with its identity.
    Records(Vec<(RecordId, Value)>),
    /// One value — or [`Value::None`] when the key holds nothing.
    ///
    /// `None` and a stored `Null` are different answers, which is the point of
    /// the value system keeping both.
    Value(Value),
    /// Keys, in order.
    Keys(Vec<RecordId>),
}

impl Outcome {
    /// The records this outcome carries, if it carries any.
    #[must_use]
    pub fn records(&self) -> Option<&[(RecordId, Value)]> {
        match self {
            Self::Records(records) => Some(records),
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
