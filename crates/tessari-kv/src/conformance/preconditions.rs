//! Checks of conditional writes: a failed precondition writes nothing, a met one applies.

use super::{CheckResult, key, value};
use crate::backend::KvBackend;
use crate::batch::WriteBatch;
use crate::error::ErrorCategory;
use crate::keyspace::Keyspace;

/// Rule 3 — a failing precondition writes nothing and reports a conflict.
pub(crate) fn failed_precondition_writes_nothing(backend: &dyn KvBackend) -> CheckResult {
    const NAME: &str = "failed-precondition-writes-nothing";
    let guarded = key(b"cas:target");
    let witness = key(b"cas:witness");
    let setup = WriteBatch::new().put(Keyspace::DATA, guarded.clone(), value(b"current"));
    if let Err(error) = backend.apply(setup) {
        return CheckResult::fail(NAME, format!("setup failed: {error}"));
    }

    let doomed = WriteBatch::new()
        .expect_value(Keyspace::DATA, guarded.clone(), value(b"stale"))
        .put(Keyspace::DATA, guarded.clone(), value(b"overwritten"))
        .put(Keyspace::DATA, witness.clone(), value(b"should-not-exist"));

    match backend.apply(doomed) {
        Ok(()) => CheckResult::fail(NAME, "batch with a false precondition was applied"),
        Err(error) if error.category() != ErrorCategory::Conflict => {
            CheckResult::fail(NAME, format!("expected conflict, got {}", error.category()))
        }
        Err(_) => {
            let target = backend.get(Keyspace::DATA, &guarded);
            let side_effect = backend.get(Keyspace::DATA, &witness);
            match (target, side_effect) {
                (Ok(Some(found)), Ok(None)) if found == value(b"current") => {
                    CheckResult::pass(NAME)
                }
                (Ok(Some(_)), Ok(Some(_))) => {
                    CheckResult::fail(NAME, "the refused batch still wrote one of its operations")
                }
                (target, side_effect) => CheckResult::fail(
                    NAME,
                    format!("target={target:?} side_effect={side_effect:?}"),
                ),
            }
        }
    }
}

/// Rule 3 — a satisfied precondition lets the batch through.
pub(crate) fn satisfied_precondition_applies(backend: &dyn KvBackend) -> CheckResult {
    const NAME: &str = "satisfied-precondition-applies";
    let target = key(b"cas:ok");
    let setup = WriteBatch::new().put(Keyspace::DATA, target.clone(), value(b"v1"));
    if let Err(error) = backend.apply(setup) {
        return CheckResult::fail(NAME, format!("setup failed: {error}"));
    }
    let batch = WriteBatch::new()
        .expect_value(Keyspace::DATA, target.clone(), value(b"v1"))
        .put(Keyspace::DATA, target.clone(), value(b"v2"));
    if let Err(error) = backend.apply(batch) {
        return CheckResult::fail(NAME, format!("apply failed: {error}"));
    }
    match backend.get(Keyspace::DATA, &target) {
        Ok(Some(found)) if found == value(b"v2") => CheckResult::pass(NAME),
        other => CheckResult::fail(NAME, format!("read back {other:?}")),
    }
}

/// An absence precondition is how uniqueness is enforced.
pub(crate) fn absent_precondition_guards_uniqueness(backend: &dyn KvBackend) -> CheckResult {
    const NAME: &str = "absent-precondition-guards-uniqueness";
    let unique = key(b"uniq:one");
    let first = WriteBatch::new()
        .expect_absent(Keyspace::INDEX, unique.clone())
        .put(Keyspace::INDEX, unique.clone(), value(b"first"));
    if let Err(error) = backend.apply(first) {
        return CheckResult::fail(NAME, format!("first insert failed: {error}"));
    }
    let second = WriteBatch::new()
        .expect_absent(Keyspace::INDEX, unique.clone())
        .put(Keyspace::INDEX, unique.clone(), value(b"second"));
    match backend.apply(second) {
        Err(error) if error.category() == ErrorCategory::Conflict => CheckResult::pass(NAME),
        Err(error) => CheckResult::fail(NAME, format!("expected conflict, got {error}")),
        Ok(()) => CheckResult::fail(NAME, "duplicate insert was allowed"),
    }
}
