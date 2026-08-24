//! Error taxonomy for the key-value substrate.
//!
//! Every error maps to exactly one [`ErrorCategory`], and every category
//! declares whether retrying can help. Callers branch on the category and the
//! retryable flag, never on the display text.
//!
//! A missing key is **not** an error here — it is [`None`] from a successful
//! read. Only failures that stop the operation appear in this enum.

use crate::key::Key;

/// Result alias for every fallible operation in this crate.
pub type Result<T> = std::result::Result<T, Error>;

/// Stable category for a substrate failure.
///
/// The category is the routing surface. Variants of [`Error`] may be added over
/// time; this set is meant to stay still.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ErrorCategory {
    /// A stated precondition did not hold, so the batch was refused.
    ///
    /// Retrying the identical batch cannot succeed — the caller must re-read and
    /// decide again.
    Conflict,
    /// The request was malformed: an inverted range, an unknown keyspace.
    Validation,
    /// The backend is momentarily unable to proceed — contention, a write stall,
    /// a lock it could not take.
    Busy,
    /// A dependency the backend needs is not reachable right now.
    Unavailable,
    /// Stored data failed an integrity check.
    ///
    /// Fatal for the operation and an operator decision for the store.
    Corruption,
    /// The store is shutting down or the target keyspace was dropped.
    ///
    /// A lifecycle condition, not a data problem.
    Lifecycle,
    /// The stored data is intact but this binary cannot interpret it — an
    /// on-disk format written by a newer version.
    ///
    /// Distinct from [`Corruption`](Self::Corruption) because the operator
    /// action is the opposite one: deploy a binary that understands the format,
    /// rather than repair or restore the data.
    Incompatible,
    /// A bug or violated invariant.
    Internal,
}

impl ErrorCategory {
    /// Stable machine-readable code, safe to expose across a boundary.
    #[must_use]
    pub const fn code(self) -> &'static str {
        match self {
            Self::Conflict => "conflict",
            Self::Validation => "validation",
            Self::Busy => "busy",
            Self::Unavailable => "unavailable",
            Self::Corruption => "corruption",
            Self::Lifecycle => "lifecycle",
            Self::Incompatible => "incompatible",
            Self::Internal => "internal",
        }
    }

    /// Whether retrying the same operation can plausibly succeed.
    ///
    /// This is a property of the category, not a suggestion to retry: the
    /// caller still owns the backoff and the attempt budget.
    #[must_use]
    pub const fn is_retryable(self) -> bool {
        match self {
            Self::Busy | Self::Unavailable => true,
            Self::Conflict
            | Self::Validation
            | Self::Corruption
            | Self::Lifecycle
            | Self::Incompatible
            | Self::Internal => false,
        }
    }
}

impl std::fmt::Display for ErrorCategory {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.code())
    }
}

/// A failure raised by the key-value substrate.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum Error {
    /// A batch precondition did not hold. Nothing was written.
    #[error("precondition failed for key {key} in keyspace {keyspace}")]
    Conflict {
        /// The keyspace the precondition addressed.
        keyspace: String,
        /// The key whose precondition failed.
        key: Key,
    },

    /// The request was rejected before touching any state.
    #[error("invalid request: {reason}")]
    Validation {
        /// What was wrong with the request.
        reason: String,
    },

    /// The named keyspace does not exist in this backend.
    #[error("unknown keyspace {keyspace}: the store was opened with a different keyspace set")]
    UnknownKeyspace {
        /// The keyspace that was asked for.
        keyspace: String,
    },

    /// The backend could not proceed right now.
    #[error("backend {backend} is busy: {reason}")]
    Busy {
        /// Which backend reported the condition.
        backend: &'static str,
        /// What it was waiting on.
        reason: String,
    },

    /// Something the backend needs is not reachable right now.
    ///
    /// Distinct from [`Busy`](Self::Busy): busy means the backend is working and
    /// cannot take more, unavailable means it could not start — a store
    /// directory already held by another process, a device that went away.
    #[error("backend {backend} is unavailable: {reason}")]
    Unavailable {
        /// Which backend reported it.
        backend: &'static str,
        /// What was not reachable.
        reason: String,
    },

    /// Stored data failed an integrity check.
    #[error("backend {backend} reported corruption: {reason}")]
    Corruption {
        /// Which backend detected it.
        backend: &'static str,
        /// What failed the check.
        reason: String,
    },

    /// The store is shutting down or the keyspace was dropped mid-operation.
    #[error("backend {backend} is not accepting work: {reason}")]
    Lifecycle {
        /// Which backend reported it.
        backend: &'static str,
        /// The lifecycle condition.
        reason: String,
    },

    /// The backend failed for a reason the caller cannot act on.
    #[error("backend {backend} failed: {reason}")]
    Backend {
        /// Which backend failed.
        backend: &'static str,
        /// What went wrong.
        reason: String,
        /// The underlying cause, when there is one.
        #[source]
        source: Option<Box<dyn std::error::Error + Send + Sync>>,
    },
}

impl Error {
    /// The category this error belongs to.
    #[must_use]
    pub const fn category(&self) -> ErrorCategory {
        match self {
            Self::Conflict { .. } => ErrorCategory::Conflict,
            Self::Validation { .. } | Self::UnknownKeyspace { .. } => ErrorCategory::Validation,
            Self::Busy { .. } => ErrorCategory::Busy,
            Self::Unavailable { .. } => ErrorCategory::Unavailable,
            Self::Corruption { .. } => ErrorCategory::Corruption,
            Self::Lifecycle { .. } => ErrorCategory::Lifecycle,
            Self::Backend { .. } => ErrorCategory::Internal,
        }
    }

    /// Stable machine-readable code for this error.
    #[must_use]
    pub const fn code(&self) -> &'static str {
        self.category().code()
    }

    /// Whether retrying the same operation can plausibly succeed.
    #[must_use]
    pub const fn is_retryable(&self) -> bool {
        self.category().is_retryable()
    }

    /// Build a validation error from any displayable reason.
    pub fn validation(reason: impl Into<String>) -> Self {
        Self::Validation {
            reason: reason.into(),
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    #[test]
    fn every_variant_maps_to_its_category() {
        let cases: Vec<(Error, ErrorCategory)> = vec![
            (
                Error::Conflict {
                    keyspace: "data".to_owned(),
                    key: Key::from_slice(b"k"),
                },
                ErrorCategory::Conflict,
            ),
            (
                Error::validation("inverted range"),
                ErrorCategory::Validation,
            ),
            (
                Error::UnknownKeyspace {
                    keyspace: "nope".to_owned(),
                },
                ErrorCategory::Validation,
            ),
            (
                Error::Busy {
                    backend: "memory",
                    reason: "write stall".to_owned(),
                },
                ErrorCategory::Busy,
            ),
            (
                Error::Unavailable {
                    backend: "memory",
                    reason: "the store directory is held by another process".to_owned(),
                },
                ErrorCategory::Unavailable,
            ),
            (
                Error::Corruption {
                    backend: "memory",
                    reason: "checksum".to_owned(),
                },
                ErrorCategory::Corruption,
            ),
            (
                Error::Lifecycle {
                    backend: "memory",
                    reason: "shutting down".to_owned(),
                },
                ErrorCategory::Lifecycle,
            ),
            (
                Error::Backend {
                    backend: "memory",
                    reason: "poisoned".to_owned(),
                    source: None,
                },
                ErrorCategory::Internal,
            ),
        ];
        for (error, expected) in cases {
            assert_eq!(error.category(), expected, "for {error}");
            assert_eq!(error.code(), expected.code());
            assert_eq!(error.is_retryable(), expected.is_retryable());
        }
    }

    #[test]
    fn only_transient_categories_are_retryable() {
        assert!(ErrorCategory::Busy.is_retryable());
        assert!(ErrorCategory::Unavailable.is_retryable());
        assert!(!ErrorCategory::Conflict.is_retryable());
        assert!(!ErrorCategory::Validation.is_retryable());
        assert!(!ErrorCategory::Corruption.is_retryable());
        assert!(!ErrorCategory::Lifecycle.is_retryable());
        assert!(!ErrorCategory::Incompatible.is_retryable());
        assert!(!ErrorCategory::Internal.is_retryable());
    }

    #[test]
    fn codes_are_stable_strings() {
        let expected = [
            (ErrorCategory::Conflict, "conflict"),
            (ErrorCategory::Validation, "validation"),
            (ErrorCategory::Busy, "busy"),
            (ErrorCategory::Unavailable, "unavailable"),
            (ErrorCategory::Corruption, "corruption"),
            (ErrorCategory::Lifecycle, "lifecycle"),
            (ErrorCategory::Incompatible, "incompatible"),
            (ErrorCategory::Internal, "internal"),
        ];
        for (category, code) in expected {
            assert_eq!(category.code(), code);
        }
    }
}
