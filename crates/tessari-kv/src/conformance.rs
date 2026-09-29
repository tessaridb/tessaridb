//! Backend conformance suite.
//!
//! Every rule documented on [`KvBackend`] has a test here, and every backend
//! runs the same set. This is deliberate: a contract asserted only against the
//! implementation that shaped it is a description, not a contract.
//!
//! A new backend implements [`KvBackend`] and calls [`run_all`] with a factory.
//! Nothing else is required of it.

mod batches;
mod preconditions;
use crate::backend::{KvBackend, ScanRequest};
use crate::batch::WriteBatch;
use crate::key::{Key, KeyRange, Value};
use crate::keyspace::Keyspace;
pub(crate) use batches::{
    batch_is_atomic_across_keyspaces, batched_first_agrees_with_scan,
    delete_range_removes_exactly_the_range, delete_removes_the_key, empty_range_returns_nothing,
    last_write_in_a_batch_wins,
};
pub(crate) use preconditions::{
    absent_precondition_guards_uniqueness, failed_precondition_writes_nothing,
    satisfied_precondition_applies,
};

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

/// A sweep answers exactly what a scan answers.
///
/// The two differ only in what the backend does with the blocks afterwards, and
/// that is invisible from here — which is the point of asserting the half that
/// is visible. A backend whose sweep dropped, reordered or truncated anything
/// would be answering a different question to save a cache, and the caller has
/// no way to notice.
fn sweep_agrees_with_scan(backend: &dyn KvBackend) -> CheckResult {
    const NAME: &str = "sweep-agrees-with-scan";
    let mut batch = WriteBatch::new();
    for n in 0..64_u32 {
        batch = batch.put(
            Keyspace::DATA,
            key(format!("swp:{n:04}").as_bytes()),
            value(format!("v{n}").as_bytes()),
        );
    }
    if let Err(error) = backend.apply(batch) {
        return CheckResult::fail(NAME, format!("apply failed: {error}"));
    }
    let request = ScanRequest::new(Keyspace::DATA, KeyRange::prefix(b"swp:"));
    let scanned = match backend.scan(&request) {
        Ok(pairs) => pairs,
        Err(error) => return CheckResult::fail(NAME, format!("scan failed: {error}")),
    };
    let swept = match backend.sweep(&request) {
        Ok(pairs) => pairs,
        Err(error) => return CheckResult::fail(NAME, format!("sweep failed: {error}")),
    };
    if swept.len() != scanned.len() {
        return CheckResult::fail(
            NAME,
            format!(
                "scan gave {} pairs and sweep gave {}",
                scanned.len(),
                swept.len()
            ),
        );
    }
    // Pair by pair rather than by length: two reads of the same range returning
    // the same count and different content is the failure a count would report
    // as agreement.
    for (at, (left, right)) in scanned.iter().zip(swept.iter()).enumerate() {
        if left != right {
            return CheckResult::fail(NAME, format!("pair {at} differs"));
        }
    }
    CheckResult::pass(NAME)
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

/// Deleting a key that is absent is not an error.
fn delete_of_absent_key_succeeds(backend: &dyn KvBackend) -> CheckResult {
    const NAME: &str = "delete-of-absent-key-succeeds";
    let batch = WriteBatch::new().delete(Keyspace::DATA, key(b"was-never-here"));
    match backend.apply(batch) {
        Ok(()) => CheckResult::pass(NAME),
        Err(error) => CheckResult::fail(NAME, format!("delete failed: {error}")),
    }
}

/// One key a range delete is checked against: where it lives, and what it should
/// hold afterwards — `None` meaning it should be gone.
type Survivor = (Keyspace, &'static [u8], Option<&'static [u8]>);

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
        "delete-range-removes-exactly-the-range",
        delete_range_removes_exactly_the_range,
    ),
    (
        "batched-first-agrees-with-scan",
        batched_first_agrees_with_scan,
    ),
    ("sweep-agrees-with-scan", sweep_agrees_with_scan),
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
