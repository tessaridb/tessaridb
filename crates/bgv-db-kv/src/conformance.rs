//! Backend conformance suite.
//!
//! Every rule documented on [`KvBackend`] has a test here, and every backend
//! runs the same set. This is deliberate: a contract asserted only against the
//! implementation that shaped it is a description, not a contract.
//!
//! A new backend implements [`KvBackend`] and calls [`run_all`] with a factory.
//! Nothing else is required of it.

use crate::backend::{KvBackend, ScanRequest};
use crate::batch::WriteBatch;
use crate::error::ErrorCategory;
use crate::key::{Key, KeyRange, Value};
use crate::keyspace::Keyspace;

/// Outcome of one conformance check.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckResult {
    /// Which rule was checked.
    pub name: &'static str,
    /// `None` when the check passed, otherwise why it failed.
    pub failure: Option<String>,
}

impl CheckResult {
    fn pass(name: &'static str) -> Self {
        Self {
            name,
            failure: None,
        }
    }

    fn fail(name: &'static str, reason: impl Into<String>) -> Self {
        Self {
            name,
            failure: Some(reason.into()),
        }
    }

    /// Whether this check passed.
    #[must_use]
    pub const fn passed(&self) -> bool {
        self.failure.is_none()
    }
}

fn key(bytes: &[u8]) -> Key {
    Key::from_slice(bytes)
}

fn value(bytes: &[u8]) -> Value {
    Value::from_slice(bytes)
}

/// Rule 5 — reading an absent key yields `None`, not an error.
fn absence_is_a_value(backend: &dyn KvBackend) -> CheckResult {
    const NAME: &str = "absence-is-a-value";
    match backend.get(Keyspace::DATA, &key(b"never-written")) {
        Ok(None) => CheckResult::pass(NAME),
        Ok(Some(found)) => CheckResult::fail(NAME, format!("expected None, found {found:?}")),
        Err(error) => CheckResult::fail(NAME, format!("expected Ok(None), got error {error}")),
    }
}

/// A value written is the value read back.
fn write_then_read(backend: &dyn KvBackend) -> CheckResult {
    const NAME: &str = "write-then-read";
    let batch = WriteBatch::new().put(Keyspace::DATA, key(b"alpha"), value(b"one"));
    if let Err(error) = backend.apply(batch) {
        return CheckResult::fail(NAME, format!("apply failed: {error}"));
    }
    match backend.get(Keyspace::DATA, &key(b"alpha")) {
        Ok(Some(found)) if found == value(b"one") => CheckResult::pass(NAME),
        Ok(other) => CheckResult::fail(NAME, format!("read back {other:?}")),
        Err(error) => CheckResult::fail(NAME, format!("read failed: {error}")),
    }
}

/// Rule 1 — keys iterate in lexicographic byte order.
fn scan_is_ordered(backend: &dyn KvBackend) -> CheckResult {
    const NAME: &str = "scan-is-ordered";
    let batch = WriteBatch::new()
        .put(Keyspace::DATA, key(b"ord:b"), value(b"2"))
        .put(Keyspace::DATA, key(b"ord:a"), value(b"1"))
        .put(Keyspace::DATA, key(b"ord:c"), value(b"3"));
    if let Err(error) = backend.apply(batch) {
        return CheckResult::fail(NAME, format!("apply failed: {error}"));
    }
    let request = ScanRequest::new(Keyspace::DATA, KeyRange::prefix(b"ord:"));
    match backend.scan(&request) {
        Ok(pairs) => {
            let keys: Vec<Key> = pairs.into_iter().map(|(k, _)| k).collect();
            let expected = vec![key(b"ord:a"), key(b"ord:b"), key(b"ord:c")];
            if keys == expected {
                CheckResult::pass(NAME)
            } else {
                CheckResult::fail(NAME, format!("order was {keys:?}"))
            }
        }
        Err(error) => CheckResult::fail(NAME, format!("scan failed: {error}")),
    }
}

/// Rule 1 — a reverse scan is exactly the forward scan reversed.
fn reverse_scan_mirrors_forward(backend: &dyn KvBackend) -> CheckResult {
    const NAME: &str = "reverse-scan-mirrors-forward";
    let batch = WriteBatch::new()
        .put(Keyspace::DATA, key(b"rev:1"), value(b"a"))
        .put(Keyspace::DATA, key(b"rev:2"), value(b"b"))
        .put(Keyspace::DATA, key(b"rev:3"), value(b"c"));
    if let Err(error) = backend.apply(batch) {
        return CheckResult::fail(NAME, format!("apply failed: {error}"));
    }
    let forward = ScanRequest::new(Keyspace::DATA, KeyRange::prefix(b"rev:"));
    let reverse = forward.clone().reversed();
    match (backend.scan(&forward), backend.scan(&reverse)) {
        (Ok(mut ascending), Ok(descending)) => {
            ascending.reverse();
            if ascending == descending {
                CheckResult::pass(NAME)
            } else {
                CheckResult::fail(NAME, "reverse scan is not the forward scan reversed")
            }
        }
        (Err(error), _) | (_, Err(error)) => {
            CheckResult::fail(NAME, format!("scan failed: {error}"))
        }
    }
}

/// A scan limit caps the number of pairs returned.
fn scan_limit_is_honoured(backend: &dyn KvBackend) -> CheckResult {
    const NAME: &str = "scan-limit-is-honoured";
    let batch = WriteBatch::new()
        .put(Keyspace::DATA, key(b"lim:1"), value(b"a"))
        .put(Keyspace::DATA, key(b"lim:2"), value(b"b"))
        .put(Keyspace::DATA, key(b"lim:3"), value(b"c"));
    if let Err(error) = backend.apply(batch) {
        return CheckResult::fail(NAME, format!("apply failed: {error}"));
    }
    let request = ScanRequest::new(Keyspace::DATA, KeyRange::prefix(b"lim:")).with_limit(2);
    match backend.scan(&request) {
        Ok(pairs) if pairs.len() == 2 => CheckResult::pass(NAME),
        Ok(pairs) => CheckResult::fail(NAME, format!("limit 2 returned {} pairs", pairs.len())),
        Err(error) => CheckResult::fail(NAME, format!("scan failed: {error}")),
    }
}

/// Rule 4 — a key written to one keyspace is invisible from another.
fn keyspaces_are_isolated(backend: &dyn KvBackend) -> CheckResult {
    const NAME: &str = "keyspaces-are-isolated";
    let shared = key(b"same-key");
    let batch = WriteBatch::new()
        .put(Keyspace::DATA, shared.clone(), value(b"in-data"))
        .put(Keyspace::INDEX, shared.clone(), value(b"in-index"));
    if let Err(error) = backend.apply(batch) {
        return CheckResult::fail(NAME, format!("apply failed: {error}"));
    }
    let from_data = backend.get(Keyspace::DATA, &shared);
    let from_index = backend.get(Keyspace::INDEX, &shared);
    let from_log = backend.get(Keyspace::LOG, &shared);
    match (from_data, from_index, from_log) {
        (Ok(Some(d)), Ok(Some(i)), Ok(None))
            if d == value(b"in-data") && i == value(b"in-index") =>
        {
            CheckResult::pass(NAME)
        }
        (d, i, l) => CheckResult::fail(NAME, format!("data={d:?} index={i:?} log={l:?}")),
    }
}

/// Rule 3 — a failing precondition writes nothing and reports a conflict.
fn failed_precondition_writes_nothing(backend: &dyn KvBackend) -> CheckResult {
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
fn satisfied_precondition_applies(backend: &dyn KvBackend) -> CheckResult {
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
fn absent_precondition_guards_uniqueness(backend: &dyn KvBackend) -> CheckResult {
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

/// Rule 2 — a batch spanning keyspaces lands entirely or not at all.
///
/// This is the property the engine depends on to write a record and the log
/// position that accounts for it together (ADR-0001).
fn batch_is_atomic_across_keyspaces(backend: &dyn KvBackend) -> CheckResult {
    const NAME: &str = "batch-is-atomic-across-keyspaces";
    let record = key(b"atomic:record");
    let position = key(b"atomic:position");
    let batch = WriteBatch::new()
        .put(Keyspace::DATA, record.clone(), value(b"payload"))
        .put(Keyspace::META, position.clone(), value(b"42"));
    if let Err(error) = backend.apply(batch) {
        return CheckResult::fail(NAME, format!("apply failed: {error}"));
    }
    match (
        backend.get(Keyspace::DATA, &record),
        backend.get(Keyspace::META, &position),
    ) {
        (Ok(Some(_)), Ok(Some(_))) => CheckResult::pass(NAME),
        (data, meta) => CheckResult::fail(NAME, format!("data={data:?} meta={meta:?}")),
    }
}

/// Deleting a key that is absent is not an error.
fn delete_of_absent_key_succeeds(backend: &dyn KvBackend) -> CheckResult {
    const NAME: &str = "delete-of-absent-key-succeeds";
    let batch = WriteBatch::new().delete(Keyspace::DATA, key(b"was-never-here"));
    match backend.apply(batch) {
        Ok(()) => CheckResult::pass(NAME),
        Err(error) => CheckResult::fail(NAME, format!("delete failed: {error}")),
    }
}

/// A delete removes the key.
fn delete_removes_the_key(backend: &dyn KvBackend) -> CheckResult {
    const NAME: &str = "delete-removes-the-key";
    let target = key(b"del:target");
    let setup = WriteBatch::new().put(Keyspace::DATA, target.clone(), value(b"v"));
    if let Err(error) = backend.apply(setup) {
        return CheckResult::fail(NAME, format!("setup failed: {error}"));
    }
    let batch = WriteBatch::new().delete(Keyspace::DATA, target.clone());
    if let Err(error) = backend.apply(batch) {
        return CheckResult::fail(NAME, format!("delete failed: {error}"));
    }
    match backend.get(Keyspace::DATA, &target) {
        Ok(None) => CheckResult::pass(NAME),
        other => CheckResult::fail(NAME, format!("key survived deletion: {other:?}")),
    }
}

/// Later operations in one batch win over earlier ones on the same key.
fn last_write_in_a_batch_wins(backend: &dyn KvBackend) -> CheckResult {
    const NAME: &str = "last-write-in-a-batch-wins";
    let target = key(b"order:within-batch");
    let batch = WriteBatch::new()
        .put(Keyspace::DATA, target.clone(), value(b"first"))
        .put(Keyspace::DATA, target.clone(), value(b"second"));
    if let Err(error) = backend.apply(batch) {
        return CheckResult::fail(NAME, format!("apply failed: {error}"));
    }
    match backend.get(Keyspace::DATA, &target) {
        Ok(Some(found)) if found == value(b"second") => CheckResult::pass(NAME),
        other => CheckResult::fail(NAME, format!("read back {other:?}")),
    }
}

/// An empty or inverted range returns nothing rather than failing.
fn empty_range_returns_nothing(backend: &dyn KvBackend) -> CheckResult {
    const NAME: &str = "empty-range-returns-nothing";
    let request = ScanRequest::new(Keyspace::DATA, KeyRange::between(key(b"z"), key(b"a")));
    match backend.scan(&request) {
        Ok(pairs) if pairs.is_empty() => CheckResult::pass(NAME),
        Ok(pairs) => CheckResult::fail(
            NAME,
            format!("inverted range returned {} pairs", pairs.len()),
        ),
        Err(error) => CheckResult::fail(NAME, format!("scan failed: {error}")),
    }
}

/// Rule 6 — a batched first-of-each answers exactly as one scan per range would.
///
/// The comparison is against [`KvBackend::scan`] rather than against a list this
/// check wrote down, because the trait defines the batched read *in terms of*
/// the single one. An override is free to be faster and is not free to be
/// different.
///
/// The ranges are chosen for the ways a seek-based override goes wrong. A range
/// with nothing in it but with keys after it is the one that matters most: a
/// seek lands on the next key in the store and does not know it has overshot,
/// so an override that forgets its end bound answers with a real pair belonging
/// to somebody else. That failure decodes, reads sensibly, and raises nothing.
fn batched_first_agrees_with_scan(backend: &dyn KvBackend) -> CheckResult {
    const NAME: &str = "batched-first-agrees-with-scan";
    let batch = WriteBatch::new()
        .put(Keyspace::DATA, key(b"b:1"), value(b"one"))
        .put(Keyspace::DATA, key(b"b:2"), value(b"two"))
        .put(Keyspace::DATA, key(b"d:1"), value(b"four"));
    if let Err(error) = backend.apply(batch) {
        return CheckResult::fail(NAME, format!("apply failed: {error}"));
    }

    let ranges = [
        // Has a hit, and a second key it must not return.
        KeyRange::prefix(b"b:"),
        // Empty, with keys on both sides of it — the overshoot case.
        KeyRange::prefix(b"c:"),
        // Bounded start, unbounded end.
        KeyRange::from_bounds(
            std::ops::Bound::Included(key(b"d:")),
            std::ops::Bound::Unbounded,
        ),
        // Unbounded start: the first key in the keyspace.
        KeyRange::from_bounds(
            std::ops::Bound::Unbounded,
            std::ops::Bound::Excluded(key(b"z")),
        ),
        // Excluded start, so the key it names is not the answer.
        KeyRange::from_bounds(
            std::ops::Bound::Excluded(key(b"b:1")),
            std::ops::Bound::Unbounded,
        ),
        // Inverted, and therefore empty however it is read.
        KeyRange::between(key(b"z"), key(b"a")),
        // Past everything.
        KeyRange::prefix(b"zz:"),
    ];

    let batched = match backend.first_of_each(Keyspace::DATA, &ranges) {
        Ok(found) => found,
        Err(error) => return CheckResult::fail(NAME, format!("first_of_each failed: {error}")),
    };
    if batched.len() != ranges.len() {
        return CheckResult::fail(
            NAME,
            format!("asked for {} ranges, got {}", ranges.len(), batched.len()),
        );
    }
    for (index, range) in ranges.iter().enumerate() {
        let request = ScanRequest::new(Keyspace::DATA, range.clone()).with_limit(1);
        let alone = match backend.scan(&request) {
            Ok(pairs) => pairs.into_iter().next(),
            Err(error) => return CheckResult::fail(NAME, format!("scan failed: {error}")),
        };
        if batched.get(index) != Some(&alone) {
            return CheckResult::fail(
                NAME,
                format!(
                    "range {index} batched to {:?} but alone reads {alone:?}",
                    batched.get(index)
                ),
            );
        }
    }

    match backend.first_of_each(Keyspace::DATA, &[]) {
        Ok(none) if none.is_empty() => CheckResult::pass(NAME),
        Ok(none) => CheckResult::fail(NAME, format!("no ranges returned {} answers", none.len())),
        Err(error) => CheckResult::fail(NAME, format!("empty slice failed: {error}")),
    }
}

/// Every check in the suite, in a stable order.
type Check = (&'static str, fn(&dyn KvBackend) -> CheckResult);

const CHECKS: &[Check] = &[
    ("absence-is-a-value", absence_is_a_value),
    ("write-then-read", write_then_read),
    ("scan-is-ordered", scan_is_ordered),
    ("reverse-scan-mirrors-forward", reverse_scan_mirrors_forward),
    ("scan-limit-is-honoured", scan_limit_is_honoured),
    ("keyspaces-are-isolated", keyspaces_are_isolated),
    (
        "failed-precondition-writes-nothing",
        failed_precondition_writes_nothing,
    ),
    (
        "satisfied-precondition-applies",
        satisfied_precondition_applies,
    ),
    (
        "absent-precondition-guards-uniqueness",
        absent_precondition_guards_uniqueness,
    ),
    (
        "batch-is-atomic-across-keyspaces",
        batch_is_atomic_across_keyspaces,
    ),
    (
        "delete-of-absent-key-succeeds",
        delete_of_absent_key_succeeds,
    ),
    ("delete-removes-the-key", delete_removes_the_key),
    ("last-write-in-a-batch-wins", last_write_in_a_batch_wins),
    ("empty-range-returns-nothing", empty_range_returns_nothing),
    (
        "batched-first-agrees-with-scan",
        batched_first_agrees_with_scan,
    ),
];

/// How many checks the suite contains.
#[must_use]
pub const fn check_count() -> usize {
    CHECKS.len()
}

/// Run every conformance check against a freshly built backend.
///
/// `make_backend` is called once per check so that no check can be influenced by
/// state another one left behind — a suite whose results depend on its own
/// ordering proves less than it appears to.
pub fn run_all<F, B>(mut make_backend: F) -> Vec<CheckResult>
where
    F: FnMut() -> B,
    B: KvBackend,
{
    CHECKS
        .iter()
        .map(|(_, check)| {
            let backend = make_backend();
            check(&backend)
        })
        .collect()
}
