//! Failures raised by the record store and its transactions.
//!
//! The categories are the substrate's, not a second vocabulary: a caller
//! already branches on [`ErrorCategory`], and two parallel taxonomies for the
//! same question is how one of them ends up unhandled.
//!
//! `Conflict` deserves a note. It is **not** a transient fault and must not be
//! retried blindly. It is a semantic outcome: another transaction committed to
//! a record this one wrote, so this one's decisions were made against a state
//! that no longer holds. The caller re-reads and decides again — which may well
//! be to do nothing.

use bgv_db_kv::ErrorCategory;
use bgv_db_types::{RecordId, Sequence};

/// Result alias for every fallible operation in this crate.
pub type Result<T> = std::result::Result<T, Error>;

/// A failure from the record store.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum Error {
    /// Another transaction committed to a record this one wrote.
    ///
    /// Under snapshot isolation the first committer wins. Nothing was written.
    #[error(
        "write conflict on record {id}: it was committed at sequence {committed} \
         after this transaction's snapshot {snapshot}"
    )]
    Conflict {
        /// The record that was written by both transactions.
        id: RecordId,
        /// The snapshot this transaction read at.
        snapshot: Sequence,
        /// The sequence the winning transaction committed at.
        committed: Sequence,
    },

    /// The commit could not claim a sequence within its attempt budget.
    ///
    /// Every attempt lost the race for the committed tail. This is contention,
    /// not a defect, and it is reported rather than retried forever.
    #[error("commit gave up after {attempts} attempts: the committed tail moved every time")]
    CommitContention {
        /// How many attempts were made.
        attempts: u32,
    },

    /// A log record was offered out of order.
    ///
    /// State is a deterministic function of the log, so a gap is not something
    /// to skip past: applying the record anyway would leave a state that no log
    /// explains, and nothing downstream could ever detect that it had.
    #[error("log gap: the next record must be {expected}, but {found} was offered")]
    LogGap {
        /// The sequence the store is ready to apply.
        expected: Sequence,
        /// The sequence that was offered instead.
        found: Sequence,
    },

    /// A catalog name is already in use at that level.
    ///
    /// Raised by the read that precedes the write. It is a courtesy, not the
    /// enforcement: uniqueness is enforced by both transactions writing the same
    /// name record, so a creation that races past this check still loses at
    /// commit with [`Error::Conflict`].
    #[error("the name {qualified} is already in use")]
    NameTaken {
        /// The qualified name, including its level tag and parent ids.
        qualified: String,
    },

    /// The parent a catalog entry was to be created under does not exist.
    #[error("no such {entity}: {id}")]
    NoSuchParent {
        /// Which level was missing — `namespace` or `database`.
        entity: &'static str,
        /// The id that resolved to nothing.
        id: u32,
    },

    /// A stored catalog entry does not have the shape a definition needs.
    ///
    /// The bytes decoded as a value, so this is not a codec failure: something
    /// wrote a well-formed value that is not a definition, which makes it an
    /// integrity problem rather than a compatibility one.
    #[error("catalog entry for a {entity} has a malformed {field}: found {found}")]
    CatalogMalformed {
        /// Which kind of entry it was.
        entity: &'static str,
        /// The field that was wrong or missing.
        field: &'static str,
        /// The type found in its place, or `none`.
        found: &'static str,
    },

    /// Every identifier at this level has been handed out.
    ///
    /// Ids are never reused after a drop, so the space is consumed by creations
    /// rather than by live entries. Retrying cannot succeed; the store needs a
    /// wider identifier, which is a format change.
    #[error("the {level} id space is exhausted")]
    IdSpaceExhausted {
        /// The level whose counter reached its end.
        level: &'static str,
    },

    /// A failure from the key-value substrate.
    #[error(transparent)]
    Kv(#[from] bgv_db_kv::Error),

    /// A failure decoding stored bytes.
    #[error(transparent)]
    Encoding(#[from] bgv_db_encoding::Error),
}

impl Error {
    /// The category this error belongs to.
    #[must_use]
    pub fn category(&self) -> ErrorCategory {
        match self {
            Self::Conflict { .. } => ErrorCategory::Conflict,
            Self::CommitContention { .. } => ErrorCategory::Busy,
            Self::LogGap { .. }
            | Self::NameTaken { .. }
            | Self::NoSuchParent { .. }
            | Self::IdSpaceExhausted { .. } => ErrorCategory::Validation,
            Self::CatalogMalformed { .. } => ErrorCategory::Corruption,
            Self::Kv(inner) => inner.category(),
            Self::Encoding(inner) => inner.category(),
        }
    }

    /// Stable machine-readable code for this error.
    #[must_use]
    pub fn code(&self) -> &'static str {
        self.category().code()
    }

    /// Whether retrying the same operation can plausibly succeed.
    ///
    /// A conflict is **not** retryable: the transaction's reads are stale, so
    /// re-running it needs a fresh snapshot and a fresh decision, not a repeat.
    #[must_use]
    pub fn is_retryable(&self) -> bool {
        self.category().is_retryable()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_conflict_is_not_retryable_and_names_both_sequences() {
        let error = Error::Conflict {
            id: RecordId::from("r"),
            snapshot: Sequence::new(5),
            committed: Sequence::new(9),
        };
        assert_eq!(error.category(), ErrorCategory::Conflict);
        assert!(!error.is_retryable());
        let text = error.to_string();
        assert!(text.contains('5'), "{text}");
        assert!(text.contains('9'), "{text}");
    }

    #[test]
    fn contention_is_retryable_because_the_transaction_itself_is_still_valid() {
        assert!(Error::CommitContention { attempts: 8 }.is_retryable());
    }
}
