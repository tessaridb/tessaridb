//! The engine's failures in the substrate's categories.

use tessari_kv::{Error, Keyspace};
use tessarikv::ErrorCategory;

use crate::error::BACKEND_NAME;

/// `error` as the layer above reads failures: by category, with the engine's own words.
pub(super) fn from_engine(error: &tessarikv::Error) -> Error {
    let reason = error.to_string();
    match error.category() {
        ErrorCategory::Validation => Error::validation(reason),
        ErrorCategory::Corruption => Error::Corruption {
            backend: BACKEND_NAME,
            reason,
        },
        ErrorCategory::Timeout => Error::Busy {
            backend: BACKEND_NAME,
            reason,
        },
        ErrorCategory::Unavailable | ErrorCategory::Io => Error::Unavailable {
            backend: BACKEND_NAME,
            reason,
        },
        ErrorCategory::ReadOnly | ErrorCategory::ShutDown => Error::Lifecycle {
            backend: BACKEND_NAME,
            reason,
        },
        _ => Error::Backend {
            backend: BACKEND_NAME,
            reason,
            source: None,
        },
    }
}

/// A write the engine's batch would not take (a key or value past its limit), as a refusal.
pub(super) fn from_batch(error: &impl std::fmt::Display) -> Error {
    Error::validation(error.to_string())
}

/// The answer for a batch whose precondition on `key` did not hold.
pub(super) fn conflict(keyspace: Keyspace, key: &tessari_kv::Key) -> Error {
    Error::Conflict {
        keyspace: keyspace.name().to_owned(),
        key: key.clone(),
    }
}
