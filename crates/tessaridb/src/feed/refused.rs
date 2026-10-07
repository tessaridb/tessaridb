//! Why a change feed would not start, or stopped.
//!
//! Every variant is a sentence an operator or a subscriber reads, and the
//! `Display` below is that sentence. The variants are what a caller or a test
//! decides on, so nothing needs to match on the words.

use core::fmt;

use crate::Error;

/// Why [`super::follow`] refused a feed, or ended one it had started.
#[derive(Debug)]
pub enum FeedRefused {
    /// The store or the session refused: a permission, a closed store, a read
    /// that failed. The session's own error, unchanged.
    Store(Error),
    /// No database is selected, so there is nothing to follow the changes to.
    NoDatabaseSelected,
    /// The selected namespace and database are not both there.
    TenancyGone,
    /// The table was named, and this session has not been granted to read it.
    TableNotGranted {
        /// The table asked for.
        table: String,
    },
    /// The table was named, and there is no table by that name.
    NoSuchTable {
        /// The table asked for.
        table: String,
    },
    /// A table the feed follows was split after the feed began, so its writes
    /// moved to logs the feed is not reading.
    SplitAfterStart,
    /// A cursor was sent to a feed over no split table, where the resume point
    /// is a sequence.
    CursorWithoutSplit {
        /// The cursor sent.
        cursor: String,
    },
    /// A log the feed would follow holds another node's writes.
    AnotherWriter {
        /// Which log: this database's, or a named shard.
        what: String,
    },
    /// The cursor counts a log this feed does not follow.
    StrayLog {
        /// The cursor sent.
        cursor: String,
    },
    /// The text is not a cursor a change of this feed carried.
    CursorUnreadable {
        /// The text sent.
        cursor: String,
    },
    /// The cursor was given by a feed over another database.
    CursorFromAnotherDatabase {
        /// The cursor sent.
        cursor: String,
    },
    /// A condition was named with no table: it is about one table's records.
    ConditionWithoutTable,
    /// The condition reads the store — a subquery, a fold, a record's lifetime
    /// — which no change carries, so it cannot be judged from one.
    ConditionReadsTheStore,
    /// The condition reads a field the subscriber may not see; judged anyway,
    /// which records arrive would say what the field holds.
    FieldNotVisible {
        /// The field.
        field: String,
    },
    /// A change that does not match had to be compared with the record as it
    /// stood just before, and that version is no longer held — a guess would
    /// leave a mirror holding a record it should have dropped.
    PreviousVersionGone {
        /// The sequence of the change.
        sequence: u64,
    },
}

impl FeedRefused {
    /// What the subscriber should do about it (ADR-0117).
    #[must_use]
    pub fn class(&self) -> tessari_types::RefusalClass {
        use tessari_types::RefusalClass;
        match self {
            Self::Store(error) => error.class(),
            Self::TableNotGranted { .. } | Self::FieldNotVisible { .. } => RefusalClass::Forbidden,
            // The table moved under the feed: follow it again, from the cursor.
            Self::SplitAfterStart => RefusalClass::Conflict,
            // This node's log is another writer's; the feed belongs elsewhere.
            Self::AnotherWriter { .. } => RefusalClass::Unavailable,
            Self::NoDatabaseSelected
            | Self::TenancyGone
            | Self::NoSuchTable { .. }
            | Self::CursorWithoutSplit { .. }
            | Self::StrayLog { .. }
            | Self::CursorUnreadable { .. }
            | Self::CursorFromAnotherDatabase { .. }
            | Self::ConditionWithoutTable
            | Self::ConditionReadsTheStore
            | Self::PreviousVersionGone { .. } => RefusalClass::Invalid,
        }
    }
}

impl fmt::Display for FeedRefused {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Store(error) => write!(formatter, "{error}"),
            Self::NoDatabaseSelected => {
                formatter.write_str("no database is selected to follow the changes to")
            }
            Self::TenancyGone => {
                formatter.write_str("that namespace and database are not both there")
            }
            Self::TableNotGranted { table } => write!(
                formatter,
                "{table:?} is not a table this session has been granted to read"
            ),
            Self::NoSuchTable { table } => write!(formatter, "no table named {table:?} to watch"),
            Self::SplitAfterStart => formatter.write_str(
                "a table this feed follows was split after it began, so its writes \
                 moved to logs the feed is not reading — subscribe again from the \
                 last change handled",
            ),
            Self::CursorWithoutSplit { cursor } => write!(
                formatter,
                "{cursor:?} is a cursor, and this feed follows no split table — resume it \
                 from the sequence of the last change handled"
            ),
            Self::AnotherWriter { what } => write!(
                formatter,
                "{what} holds another node's writes, and a change feed over a split \
                 table follows one writer's logs — follow it on the node that writes them"
            ),
            Self::StrayLog { cursor } => write!(
                formatter,
                "{cursor:?} counts a log this feed does not follow — send the cursor this \
                 feed's own last change carried"
            ),
            Self::CursorUnreadable { cursor } => write!(
                formatter,
                "{cursor:?} is not a cursor a change of this feed carried"
            ),
            Self::CursorFromAnotherDatabase { cursor } => write!(
                formatter,
                "{cursor:?} was given by a feed over another database, and its positions mean \
                 nothing in this one"
            ),
            Self::ConditionWithoutTable => formatter
                .write_str("a condition is about one table's records — name the table it narrows"),
            Self::ConditionReadsTheStore => formatter.write_str(
                "this condition reads the store, and a feed judges each change by the record \
                 it carries — compare the record's own fields",
            ),
            Self::FieldNotVisible { field } => write!(
                formatter,
                "the condition reads {field:?}, which this session may not see"
            ),
            Self::PreviousVersionGone { sequence } => write!(
                formatter,
                "the change at {sequence} must be compared with the record as it stood before \
                 it, and that version has been reclaimed — subscribe again from the present"
            ),
        }
    }
}

impl std::error::Error for FeedRefused {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Store(error) => Some(error),
            _ => None,
        }
    }
}

impl From<Error> for FeedRefused {
    fn from(error: Error) -> Self {
        Self::Store(error)
    }
}

impl From<tessari_storage::Error> for FeedRefused {
    fn from(error: tessari_storage::Error) -> Self {
        Self::Store(Error::from(error))
    }
}
