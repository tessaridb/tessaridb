//! Where a declared feed's initial scan has reached (FEAT-S1 D8).
//!
//! One system row per (table, feed) carrying `WITH INITIAL SCAN`, written by
//! the defining commit itself. That placement is the load-bearing decision:
//! the row's key version is the define commit's sequence, which is exactly
//! the snapshot `S` the scan must anchor at — a record written after `S`
//! already has its live message, and a record whose latest version is at or
//! before `S` has none. No second write is needed to carry `S`, so there is
//! no crash window between the declaration and its snapshot; the first tick
//! reads the version off the swept key, stores it in the row, and from then
//! on the stored value governs across restarts.

use std::collections::BTreeMap;
use std::ops::Bound;

use tessari_encoding::{
    RecordKey, RecordValue, StampedValue, StoreKey, StoreValue, decode_payload, encode_payload,
};
use tessari_kv::{Key, KeyRange, ScanDirection, ScanRequest};
use tessari_types::{DatabaseId, NamespaceId, Number, RecordId, Sequence, TableId, Value};

use crate::catalog::system;
use crate::error::{Error, Result};
use crate::store::Store;
use crate::transaction::Transaction;

/// How many keys one scan batch examines.
///
/// The bound counts keys examined, not records emitted: a table most of
/// whose rows changed after the snapshot emits little from a batch and must
/// still bound the work it walked to find out.
const SCAN_BATCH: usize = 256;

const ENTITY: &str = "feed scan";
const FIELD_SNAPSHOT: &str = "snapshot";
const FIELD_NEXT: &str = "next";
const FIELD_DONE: &str = "done";

/// One feed's scan job, as its system row holds it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FeedScanState {
    /// The sequence the scan emits against; zero means "not yet resolved —
    /// the row's own key version is the define commit's sequence".
    pub snapshot: u64,
    /// Where the next batch resumes: the raw lower-bound key (the smallest
    /// key above the last decided identity's whole version range), absent at
    /// the start and absent once the table is exhausted.
    pub next: Option<Vec<u8>>,
    /// Whether the walk reached the end of the table.
    pub done: bool,
}

impl FeedScanState {
    /// A job just defined: snapshot unresolved, nothing scanned, not done.
    #[must_use]
    pub fn pending() -> Self {
        Self {
            snapshot: 0,
            next: None,
            done: false,
        }
    }

    /// The value written into the system row.
    #[must_use]
    pub fn to_value(&self) -> Value {
        let mut fields = BTreeMap::new();
        if self.snapshot != 0 {
            fields.insert(
                FIELD_SNAPSHOT.to_owned(),
                Value::Number(Number::Integer(
                    i64::try_from(self.snapshot).unwrap_or(i64::MAX),
                )),
            );
        }
        if let Some(next) = &self.next {
            fields.insert(FIELD_NEXT.to_owned(), Value::Bytes(next.clone()));
        }
        if self.done {
            fields.insert(FIELD_DONE.to_owned(), Value::Bool(true));
        }
        Value::Object(fields)
    }

    /// Read a job back.
    ///
    /// # Errors
    ///
    /// Returns [`Error::CatalogMalformed`] when a field holds the wrong type.
    pub fn from_value(value: &Value) -> Result<Self> {
        let Value::Object(fields) = value else {
            return Err(malformed(FIELD_SNAPSHOT, value));
        };
        let snapshot = match fields.get(FIELD_SNAPSHOT) {
            None => 0,
            Some(Value::Number(Number::Integer(held))) => {
                u64::try_from(*held).map_err(|_| malformed(FIELD_SNAPSHOT, value))?
            }
            Some(other) => return Err(malformed(FIELD_SNAPSHOT, other)),
        };
        let next = match fields.get(FIELD_NEXT) {
            None => None,
            Some(Value::Bytes(held)) => Some(held.clone()),
            Some(other) => return Err(malformed(FIELD_NEXT, other)),
        };
        Ok(Self {
            snapshot,
            next,
            done: matches!(fields.get(FIELD_DONE), Some(Value::Bool(true))),
        })
    }
}

fn malformed(field: &'static str, found: &Value) -> Error {
    Error::CatalogMalformed {
        entity: ENTITY,
        field,
        found: found.type_name(),
    }
}

/// The system-row identity of one (table, feed) pair.
///
/// Feed names are unique on their table only, so the table id rides in the
/// id; the byte form is length-prefixed, so the pair encodes without
/// ambiguity.
fn scan_id(table: TableId, feed: &str) -> RecordId {
    let mut raw = table.get().to_be_bytes().to_vec();
    raw.extend_from_slice(feed.as_bytes());
    RecordId::Bytes(raw)
}

/// The records one batch of an initial scan emits (FEAT-S1 D8).
///
/// A batch is bounded by keys examined, not records emitted: most of a
/// table's rows may postdate the snapshot, and the walk must bound what it
/// looked at to find out, the stride the expiry passes keep.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ScannedBatch {
    /// The rows to emit: the identity and the record as it stands.
    pub records: Vec<(RecordId, Value)>,
    /// Where the next batch resumes; absent when the table is exhausted.
    pub resume: Option<Vec<u8>>,
}

impl Transaction<'_> {
    /// Where one feed's scan has reached, if the job exists.
    ///
    /// # Errors
    ///
    /// Returns an error when the row cannot be read or decoded.
    pub async fn feed_scan_state(
        &self,
        table: TableId,
        feed: &str,
    ) -> Result<Option<FeedScanState>> {
        let address = system::address(system::FEED_SCAN_STATES, scan_id(table, feed));
        match self.get(&address).await? {
            Some(bytes) => Ok(Some(FeedScanState::from_value(&decode_payload(&bytes)?)?)),
            None => Ok(None),
        }
    }

    /// Record where one feed's scan has reached, in the same transaction as
    /// the batch it describes.
    ///
    /// # Errors
    ///
    /// Returns an error when the state cannot be encoded.
    pub fn put_feed_scan_state(
        &mut self,
        table: TableId,
        feed: &str,
        state: &FeedScanState,
    ) -> Result<()> {
        let payload = encode_payload(&state.to_value()).into_bytes();
        self.put(
            system::address(system::FEED_SCAN_STATES, scan_id(table, feed)),
            payload,
        );
        Ok(())
    }

    /// Remove one feed's scan job: `DROP FEED` leaves no state a redefine
    /// would have to reconcile.
    pub fn forget_feed_scan(&mut self, table: TableId, feed: &str) {
        self.delete(system::address(
            system::FEED_SCAN_STATES,
            scan_id(table, feed),
        ));
    }
}

impl Store {
    /// The sequence the feed's scan-state row was written at — the define
    /// commit's sequence, which is the snapshot the scan anchors at.
    ///
    /// # Errors
    ///
    /// Returns an error when the row is missing or its key cannot be decoded.
    pub async fn feed_scan_defined_at(&self, table: TableId, feed: &str) -> Result<Sequence> {
        let prefix = RecordKey::versions_prefix(
            system::SYSTEM_NAMESPACE,
            system::SYSTEM_DATABASE,
            system::FEED_SCAN_STATES,
            &scan_id(table, feed),
        );
        let found = self
            .backend()
            .scan(&ScanRequest {
                keyspace: RecordKey::keyspace(),
                range: KeyRange::prefix(&prefix),
                direction: ScanDirection::Forward,
                limit: Some(1),
            })
            .await?;
        let Some((key, _)) = found.first() else {
            return Err(Error::CatalogMalformed {
                entity: ENTITY,
                field: "row",
                found: "missing",
            });
        };
        Ok(RecordKey::decode(key.as_slice())?.version)
    }

    /// Examine one bounded batch of a table for its initial scan: every row
    /// whose newest kept version was written at or before `at`, in key order
    /// from `from` (or the table's start), with the position a following
    /// batch resumes at.
    ///
    /// Versions of one identity are adjacent, newest first, so the first
    /// entry decides: an intent says nothing and the version under it does
    /// (ADR-0112 D5), a tombstone or a version after `at` is skipped — the
    /// live messages carry those.
    ///
    /// # Errors
    ///
    /// Returns an error when the backend fails or a key or value cannot be
    /// decoded.
    pub async fn scan_feed_batch(
        &self,
        namespace: NamespaceId,
        database: DatabaseId,
        table: TableId,
        from: Option<&[u8]>,
        at: Sequence,
    ) -> Result<ScannedBatch> {
        let prefix = RecordKey::table_prefix(namespace, database, table);
        let end = KeyRange::prefix(&prefix).end().clone();
        let start = from.map_or_else(|| Key::from_slice(&prefix), Key::from_slice);
        let range = KeyRange::from_bounds(Bound::Included(start), end);
        let pairs = self
            .backend()
            .scan(&ScanRequest {
                keyspace: RecordKey::keyspace(),
                range,
                direction: ScanDirection::Forward,
                limit: Some(SCAN_BATCH),
            })
            .await?;
        let mut batch = ScannedBatch::default();
        let mut seen: Option<RecordId> = None;
        let mut decided: Option<RecordId> = None;
        for (key, value) in &pairs {
            let decoded = RecordKey::decode(key.as_slice())?;
            if seen.as_ref() == Some(&decoded.id) {
                continue;
            }
            let stored = StampedValue::decode(value.as_slice())?;
            // An intent says nothing about whether the record is there; the
            // version under it, next in this walk, does (ADR-0112 D5).
            if crate::intents::is_intent(&stored) {
                continue;
            }
            seen = Some(decoded.id.clone());
            decided = Some(decoded.id.clone());
            if decoded.version > at {
                // Written after the define: its live message carries it.
                continue;
            }
            let RecordValue::Present(payload) = stored.into_value() else {
                continue;
            };
            batch.records.push((decoded.id, decode_payload(&payload)?));
        }
        if pairs.len() < SCAN_BATCH {
            return Ok(batch);
        }
        // The walk continues. Resume past the last decided identity, not the
        // last key read: a batch can end mid-identity (its older versions
        // follow), and resuming there would decide it a second time. The
        // bound is the smallest key above the identity's whole version range
        // — the exclusive end of its prefix range — which a `++ 0x00` on the
        // prefix would not give: that admits every key of the identity, whose
        // version suffix sorts above it.
        let bound = match decided {
            Some(id) => {
                let id_prefix = RecordKey::versions_prefix(namespace, database, table, &id);
                match KeyRange::prefix(&id_prefix).end() {
                    Bound::Excluded(upper) => upper.as_slice().to_vec(),
                    Bound::Unbounded => return Ok(batch),
                    Bound::Included(_) => unreachable!("a prefix range ends excluded"),
                }
            }
            // Every entry was an intent: fall back to the expiry pass's
            // bound — everything sorts after the last key read.
            None => pairs
                .last()
                .map_or(prefix, |(key, _)| key.as_slice().to_vec())
                .into_iter()
                .chain(std::iter::once(0))
                .collect(),
        };
        batch.resume = Some(bound);
        Ok(batch)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::FeedScanState;

    #[test]
    fn a_state_reads_back_as_written() {
        // The snapshot clamps at i64::MAX on the way out, the convention the
        // catalog keeps for every count; a real sequence never comes near it.
        for (snapshot, next, done) in [
            (0, None, false),
            (0, None, true),
            (41, Some(vec![1, 2, 0]), false),
            (i64::MAX as u64, Some(Vec::new()), true),
        ] {
            let held = FeedScanState {
                snapshot,
                next: next.clone(),
                done,
            };
            assert_eq!(FeedScanState::from_value(&held.to_value()).unwrap(), held);
        }
    }
}
