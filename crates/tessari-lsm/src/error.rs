//! Translating engine failures into the substrate's taxonomy.
//!
//! The substrate already has a category vocabulary that callers branch on, so
//! this crate adds none of its own. Its whole job is to be the single place
//! where an engine status is read, so that no code above ever inspects one.
//!
//! Two of the translations look at the message text, and that is deliberate.
//! The engine reports "this directory is already held" and "this store has no
//! such region" as ordinary I/O and invalid-argument failures, and the operator
//! action for each is completely different from a generic failure of the same
//! kind. Reading the text *here*, once, at the boundary, is what lets every
//! caller branch on the category instead — the rule is that callers never parse
//! a message, not that nobody ever may.

use rocksdb::ErrorKind;
use tessari_kv::{Error, Keyspace};

/// The name this backend reports in errors and logs.
pub(crate) const BACKEND_NAME: &str = "lsm";

/// Map an engine failure onto the substrate's taxonomy.
pub(crate) fn from_engine(error: &rocksdb::Error) -> Error {
    let reason = error.to_string();
    match error.kind() {
        ErrorKind::Corruption => Error::Corruption {
            backend: BACKEND_NAME,
            reason,
        },
        // Working, and cannot take more right now.
        ErrorKind::Busy | ErrorKind::TryAgain | ErrorKind::TimedOut | ErrorKind::Incomplete => {
            Error::Busy {
                backend: BACKEND_NAME,
                reason,
            }
        }
        // Could not start, or was stopped from outside.
        ErrorKind::IOError | ErrorKind::Expired | ErrorKind::Aborted => Error::Unavailable {
            backend: BACKEND_NAME,
            reason,
        },
        ErrorKind::ShutdownInProgress | ErrorKind::ColumnFamilyDropped => Error::Lifecycle {
            backend: BACKEND_NAME,
            reason,
        },
        ErrorKind::InvalidArgument | ErrorKind::NotSupported => Error::validation(reason),
        // `NotFound` never reaches here: an absent key is `Ok(None)` on the read
        // path and never an engine error, so seeing it means something else
        // returned it and the caller cannot act on the difference.
        _ => Error::Backend {
            backend: BACKEND_NAME,
            reason,
            source: None,
        },
    }
}

/// Map an open failure, where two shapes deserve their own words.
pub(crate) fn from_open(error: &rocksdb::Error, path: &std::path::Path) -> Error {
    let text = error.to_string();
    if is_lock_held(&text) {
        return Error::Unavailable {
            backend: BACKEND_NAME,
            reason: format!(
                "the store at {} is already open in another process; \
                 the lock is never taken away from its owner",
                path.display()
            ),
        };
    }
    from_engine(error)
}

/// A store whose region set is not the one this build expects.
///
/// This is not a case for creating the missing region: a region that should be
/// there and is not means the store was written by something with a different
/// idea of what it contains, and starting anyway would serve empty results from
/// a region that merely looks new.
pub(crate) fn missing_region(missing: &[Keyspace], path: &std::path::Path) -> Error {
    let names: Vec<&str> = missing.iter().map(|keyspace| keyspace.name()).collect();
    Error::validation(format!(
        "the store at {} is missing region(s) {}; it was not created by this build \
         and will not be extended in place",
        path.display(),
        names.join(", ")
    ))
}

/// Whether the failure text describes the directory lock.
fn is_lock_held(text: &str) -> bool {
    let lowered = text.to_lowercase();
    lowered.contains("lock") && (lowered.contains("hold") || lowered.contains("held"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tessari_kv::ErrorCategory;

    #[test]
    fn the_directory_lock_message_is_recognised_in_the_forms_the_engine_uses() {
        assert!(is_lock_held("IO error: lock hold by current process"));
        assert!(is_lock_held(
            "IO error: While lock file: /tmp/db/LOCK: Resource temporarily unavailable, \
             lock is held by another process"
        ));
        assert!(!is_lock_held("IO error: No space left on device"));
    }

    #[test]
    fn a_missing_region_is_a_refusal_to_open_and_names_what_is_missing() {
        let error = missing_region(&[Keyspace::LOG], std::path::Path::new("/tmp/store"));
        assert_eq!(error.category(), ErrorCategory::Validation);
        let text = error.to_string();
        assert!(text.contains("log"), "{text}");
        assert!(text.contains("/tmp/store"), "{text}");
    }
}
