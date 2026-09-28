//! Checks of batches, deletes and ranges.

use super::{CheckResult, Survivor, key, value};
use crate::backend::{KvBackend, ScanRequest};
use crate::batch::WriteBatch;
use crate::key::KeyRange;
use crate::keyspace::Keyspace;

/// Rule 2 — a batch spanning keyspaces lands entirely or not at all.
///
/// This is the property the engine depends on to write a record and the log
/// position that accounts for it together (ADR-0001).
pub(crate) fn batch_is_atomic_across_keyspaces(backend: &dyn KvBackend) -> CheckResult {
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

/// A delete removes the key.
pub(crate) fn delete_removes_the_key(backend: &dyn KvBackend) -> CheckResult {
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
pub(crate) fn last_write_in_a_batch_wins(backend: &dyn KvBackend) -> CheckResult {
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

/// Rule 7 — a range delete removes exactly the range.
///
/// The neighbours are the check. A backend that translates the range into its
/// engine's own half-open span and gets a bound one byte wrong deletes a key
/// nobody named and returns `Ok(())`, and nothing downstream can tell that from
/// a key that was never written. So `a` sits immediately below the range and `d`
/// immediately above it, and both are read back afterwards.
///
/// The second delete is the idempotence half: a prune that is retried after a
/// crash re-issues the same range, and a backend that treats an already-empty
/// range as an error would turn recovery into a failure.
pub(crate) fn delete_range_removes_exactly_the_range(backend: &dyn KvBackend) -> CheckResult {
    const NAME: &str = "delete-range-removes-exactly-the-range";
    let batch = WriteBatch::new()
        .put(Keyspace::LOG, key(b"a"), value(b"below"))
        .put(Keyspace::LOG, key(b"b"), value(b"inside"))
        .put(Keyspace::LOG, key(b"c"), value(b"inside"))
        .put(Keyspace::LOG, key(b"d"), value(b"above"))
        .put(Keyspace::DATA, key(b"b"), value(b"another keyspace"));
    if let Err(error) = backend.apply(batch) {
        return CheckResult::fail(NAME, format!("setup failed: {error}"));
    }
    let range = KeyRange::between(key(b"b"), key(b"d"));
    if let Err(error) = backend.delete_range(Keyspace::LOG, &range) {
        return CheckResult::fail(NAME, format!("delete_range failed: {error}"));
    }
    if let Err(error) = backend.delete_range(Keyspace::LOG, &range) {
        return CheckResult::fail(NAME, format!("a repeated delete_range failed: {error}"));
    }
    let expected: &[Survivor] = &[
        (Keyspace::LOG, b"a", Some(b"below")),
        (Keyspace::LOG, b"b", None),
        (Keyspace::LOG, b"c", None),
        (Keyspace::LOG, b"d", Some(b"above")),
        (Keyspace::DATA, b"b", Some(b"another keyspace")),
    ];
    for (keyspace, bytes, wanted) in expected {
        let found = match backend.get(*keyspace, &key(bytes)) {
            Ok(found) => found,
            Err(error) => return CheckResult::fail(NAME, format!("read back failed: {error}")),
        };
        let wanted = wanted.map(value);
        if found != wanted {
            return CheckResult::fail(
                NAME,
                format!(
                    "{keyspace:?} {} read back {found:?}, wanted {wanted:?}",
                    String::from_utf8_lossy(bytes)
                ),
            );
        }
    }
    CheckResult::pass(NAME)
}

/// An empty or inverted range returns nothing rather than failing.
pub(crate) fn empty_range_returns_nothing(backend: &dyn KvBackend) -> CheckResult {
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
pub(crate) fn batched_first_agrees_with_scan(backend: &dyn KvBackend) -> CheckResult {
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
