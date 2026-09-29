//! Reading a table without an index.
//!
//! The batched walk every other read is measured against: it costs the table,
//! and it is what the planner falls back to when no access path serves the
//! predicate.

mod records;
mod spans;
use std::ops::{Bound, ControlFlow};

use tessari_constants::RANGE_SCAN_BATCH_ENTRIES;
use tessari_encoding::{RecordKey, RecordValue};
use tessari_kv::{Key, KeyRange, Keyspace, ScanDirection, ScanRequest};
use tessari_types::{DatabaseId, NamespaceId, RecordId, TableId};

use super::Transaction;
use super::address::{after, resuming_after};

/// A span of record identities, as the walk carries it.
///
/// Its own type rather than three parameters, because `lower`, `upper` and
/// whether the upper is inside are one fact and are wrong together: a walk
/// given the first two and not the third silently answers a half-open span as
/// a closed one, and nothing in the answer says which it was.
#[derive(Debug, Clone, Copy)]
struct Span<'a> {
    /// The first identity in the span, which is always inside it.
    lower: &'a RecordId,
    /// The last, which is inside it only when `inclusive`.
    upper: &'a RecordId,
    /// Whether `upper` is itself in the span.
    inclusive: bool,
}

impl Span<'_> {
    /// Whether an identity is inside this span.
    fn holds(&self, id: &RecordId) -> bool {
        id >= self.lower
            && if self.inclusive {
                id <= self.upper
            } else {
                id < self.upper
            }
    }
}
use crate::error::Result;

/// Whether a walk's blocks are worth keeping.
///
/// A read that serves a query wants its blocks cached, because the next query is
/// likely to want them. A read that walks a whole table once to check or rebuild
/// something does not: it touches every block, asks for none of them again, and a
/// cache that keeps them has evicted what the store is actually serving.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Reading {
    /// The ordinary case.
    Serving,
    /// A one-shot verification or build pass.
    Sweep,
}

/// What one walk of a table is being asked for.
///
/// The four readers below differ only in these fields, and they travel together
/// because a walk is one shape rather than four loose arguments — which is also
/// what keeps the shared walk's signature readable as the set grows.
/// Part of a table's identity order whose ends may be open (G033).
///
/// A shard's span is open at the table's edges, which a [`Span`] — the
/// language's, with both ends always written — cannot say.
#[derive(Debug, Clone, Copy, Default)]
pub struct Window<'a> {
    /// The first identity inside, or `None` for the start of the table.
    pub from: Option<&'a RecordId>,
    /// Where the window stops and whether that identity is inside, or `None`
    /// for the end of the table.
    pub to: Option<(&'a RecordId, bool)>,
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct Walk<'a> {
    /// At least this many records, or every one there is.
    bound: Option<usize>,
    /// Start past this record's own key.
    anchor: Option<&'a RecordId>,
    /// Stop at this identity span.
    span: Option<Span<'a>>,
    /// A window with open ends (G033): the first identity it holds, and where
    /// it stops with whether that identity is inside. Beside `span` rather than
    /// instead of it, because a span is the language's and both of its ends are
    /// always written.
    from: Option<&'a RecordId>,
    to: Option<(&'a RecordId, bool)>,
    /// Whether the blocks this walk reads are worth keeping.
    reading: Reading,
}

impl Default for Walk<'_> {
    fn default() -> Self {
        Self {
            bound: None,
            anchor: None,
            span: None,
            from: None,
            to: None,
            reading: Reading::Serving,
        }
    }
}

impl Transaction<'_> {
    /// Every live record of one table, as of this transaction's snapshot.
    ///
    /// Records come back in key order, with this transaction's own uncommitted
    /// writes folded in, and deleted records left out — a tombstone is a version
    /// like any other on disk, and a caller asking what is in a table does not
    /// want to hear about the rows that are not.
    ///
    /// **This reads the whole table.** It exists because the catalog is a table
    /// and the catalog is small. A read that knows how many records it needs
    /// asks [`Self::first_records_of`] instead; calling this one on a table of
    /// unbounded size is a mistake this signature cannot prevent and this
    /// sentence is the warning.
    ///
    /// # Errors
    ///
    /// Returns an error when the backend fails or stored bytes cannot be
    /// decoded.
    pub fn scan_table(
        &self,
        namespace: NamespaceId,
        database: DatabaseId,
        table: TableId,
    ) -> Result<Vec<(RecordId, Vec<u8>)>> {
        self.table_records(namespace, database, table, Walk::default())
    }

    /// Every live record of one table, for a pass that will not read them again.
    ///
    /// Answers exactly what [`Self::scan_table`] answers. The difference is that
    /// the backend is told not to keep the blocks, because the reads that walk a
    /// whole table to check or rebuild something — an index build, the
    /// retroactive tightening pass, `CHECK TABLE` — touch every block once and
    /// ask for none of them again. A cache that keeps them has evicted the
    /// working set the store is serving, and serving latency degrades for
    /// minutes after the statement returned with nothing to point at.
    ///
    /// # Errors
    ///
    /// Returns an error when the backend fails or stored bytes cannot be
    /// decoded.
    pub fn sweep_table(
        &self,
        namespace: NamespaceId,
        database: DatabaseId,
        table: TableId,
    ) -> Result<Vec<(RecordId, Vec<u8>)>> {
        self.table_records(
            namespace,
            database,
            table,
            Walk {
                reading: Reading::Sweep,
                ..Walk::default()
            },
        )
    }

    /// Every live record of one table, handed over as the walk finds them.
    ///
    /// The streaming twin of [`Self::scan_table`], and one difference is the
    /// whole of it: `hand` may answer `Break`, and the walk then stops where it
    /// stands instead of after the table.
    ///
    /// # Why this exists beside a method that already reads a table
    ///
    /// A read whose answer count is its `LIMIT` pushes that bound into the
    /// source (ADR-0013) and costs the bound. A read with a `WHERE` cannot: the
    /// bound counts records that **match** and the source counts records that
    /// **exist**, so no number can be handed down. What the caller has instead
    /// is the consumer's `Break`, which every stage above the source already
    /// honours — and which `scan_table` cannot deliver, because it builds the
    /// whole table's payloads before the caller evaluates its first predicate.
    /// Measured before this was written: a match found at the third of a hundred
    /// thousand records cost 83.3 ms, a match at the last cost 82.9, and no
    /// match at all cost 84.3. The position of the match did not change the
    /// cost, which is what a source that cannot be stopped looks like.
    ///
    /// The **answer** is still a materialised value, so ADR-0013's refusal of
    /// streaming stands untouched: nothing is handed to a caller while a
    /// snapshot is open, and the snapshot's life gets shorter rather than longer
    /// because the source stops. What streams is the source's own buffering, one
    /// level below the answer, exactly as ADR-0014 already did for decoding.
    ///
    /// # What `hand` receives
    ///
    /// The transaction itself, because a caller that stops early is deciding
    /// something — testing a condition, feeding a consumer — and both need it.
    /// Records arrive in key order with this transaction's own uncommitted
    /// writes merged into that order, deleted records left out, and a record
    /// this transaction has written answered from the write rather than from the
    /// committed version underneath it.
    ///
    /// # Errors
    ///
    /// Returns an error when the backend fails, when stored bytes cannot be
    /// decoded, or when `hand` itself fails.
    pub fn walk_table<F, E>(
        &mut self,
        namespace: NamespaceId,
        database: DatabaseId,
        table: TableId,
        mut hand: F,
    ) -> std::result::Result<(), E>
    where
        F: FnMut(&mut Self, RecordId, Vec<u8>) -> std::result::Result<ControlFlow<()>, E>,
        E: From<crate::error::Error>,
    {
        let prefix = RecordKey::table_prefix(namespace, database, table);
        // Copied out before the walk rather than read in step with it: `hand`
        // takes the transaction, so nothing may hold a borrow of it across the
        // call. There are as many of these as this transaction has written to
        // this table, which for the read that motivates this walk is none.
        // The streaming twin opens at the floor for the reason the collecting
        // walk does, and it is the same call so the two cannot drift about where
        // a series table begins.
        let floor = self.series_floor(namespace, table).map_err(E::from)?;
        let mut pending = self
            .writes
            .iter()
            .filter(|(address, _)| {
                address.namespace == namespace
                    && address.database == database
                    && address.table == table
                    // Judged against the floor for the reason the collecting
                    // walk's are: a record this transaction wrote below the
                    // floor is one the same read would not have found a moment
                    // earlier.
                    && floor.as_ref().is_none_or(|floor| &address.id >= floor)
            })
            .map(|(address, value)| (address.id.clone(), value.clone()))
            .collect::<Vec<_>>()
            .into_iter()
            .peekable();

        let mut resolved: Option<RecordId> = None;
        let mut from = match &floor {
            Some(floor) => RecordKey::versions_prefix(namespace, database, table, floor),
            None => prefix.clone(),
        };
        let end = after(prefix);
        loop {
            // Batched although the walk names no bound. `table_records` asks for
            // everything in one request because it is going to hold everything
            // anyway; here a single unbounded request would read the table
            // before the first record could ask to stop, which is the cost this
            // walk exists to remove.
            let batch = self.batch_of(&from, &end, Some(RANGE_SCAN_BATCH_ENTRIES))?;
            let last = batch.last().map(|(key, _)| key.as_slice().to_vec());
            let entries = batch.len();
            for (id, value) in self.settled(batch, &mut resolved)? {
                // Pending writes sorting before this record come first, so the
                // records arrive in key order whether they are committed or not
                // — the order `scan_table` answers in, and therefore the order a
                // bound above truncates.
                while let Some((waiting, written)) = pending.next_if(|(waiting, _)| waiting < &id) {
                    if let RecordValue::Present(payload) = written
                        && hand(self, waiting, payload)?.is_break()
                    {
                        return Ok(());
                    }
                }
                // A record this transaction has written is answered from the
                // write and not from the committed version beneath it —
                // including when the write is a tombstone, which removes it.
                let value = match pending.next_if(|(waiting, _)| waiting == &id) {
                    Some((_, written)) => written,
                    None => value,
                };
                if let RecordValue::Present(payload) = value
                    && hand(self, id, payload)?.is_break()
                {
                    return Ok(());
                }
            }
            // A batch shorter than the one asked for is the end of the table.
            if entries < RANGE_SCAN_BATCH_ENTRIES {
                break;
            }
            let Some(last) = last else {
                break;
            };
            from = resuming_after(last);
        }
        // Whatever the committed walk never reached: records written in this
        // transaction that sort after the last one on disk.
        for (waiting, written) in pending {
            if let RecordValue::Present(payload) = written
                && hand(self, waiting, payload)?.is_break()
            {
                return Ok(());
            }
        }
        Ok(())
    }

    /// One batched scan of a span, up to `limit` entries.
    ///
    /// The batching is the backend's request limit rather than the caller's, so a
    /// caller asking for everything does not ask for it in one allocation.
    pub(super) fn scan_once(
        &self,
        keyspace: Keyspace,
        span: KeyRange,
        limit: usize,
    ) -> Result<Vec<(Key, tessari_kv::Value)>> {
        let mut taken = Vec::new();
        let mut from = match span.start() {
            Bound::Included(key) => key.as_slice().to_vec(),
            Bound::Excluded(key) => resuming_after(key.as_slice().to_vec()),
            Bound::Unbounded => Vec::new(),
        };
        while taken.len() < limit {
            let batch = self.store.backend().scan(&ScanRequest {
                keyspace,
                range: KeyRange::from_bounds(Bound::Included(Key::from(from)), span.end().clone()),
                direction: ScanDirection::Forward,
                limit: Some(RANGE_SCAN_BATCH_ENTRIES.min(limit.saturating_sub(taken.len()))),
            })?;
            let full = batch.len() >= RANGE_SCAN_BATCH_ENTRIES;
            let last = batch.last().map(|(key, _)| key.as_slice().to_vec());
            taken.extend(batch);
            let Some(last) = last.filter(|_| full) else {
                break;
            };
            from = resuming_after(last);
        }
        Ok(taken)
    }
}
