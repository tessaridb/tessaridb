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
