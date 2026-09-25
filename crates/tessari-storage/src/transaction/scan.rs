//! Reading a table without an index.
//!
//! The batched walk every other read is measured against: it costs the table,
//! and it is what the planner falls back to when no access path serves the
//! predicate.

use std::collections::BTreeMap;
use std::ops::{Bound, ControlFlow};

use tessari_constants::RANGE_SCAN_BATCH_ENTRIES;
use tessari_encoding::{RecordKey, RecordValue, StampedValue, StoreKey, StoreValue};
use tessari_kv::{Key, KeyRange, Keyspace, ScanDirection, ScanRequest};
use tessari_types::{DatabaseId, NamespaceId, RecordId, TableId};

use super::address::{after, resuming_after};
use super::{RecordAddress, Transaction};

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
enum Reading {
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
struct Walk<'a> {
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

    /// The live records of one table whose identity sorts after `anchor`.
    ///
    /// The seek behind a cursor. A record's key is its table prefix followed by
    /// its identity, so "after this record" is a **position in the keyspace**
    /// and not a predicate: the walk starts past the anchor's own versions and
    /// the records before it are never read at all. That is the whole difference
    /// between a cursor and an offset, and it is why this is a method here
    /// rather than a filter above.
    ///
    /// The anchor itself need not exist. It names a position, and a position is
    /// well defined whether or not something sits on it — which is what lets a
    /// page walk survive the deletion of the record it resumed from.
    ///
    /// `bound` carries the same looser-than-it-looks contract as
    /// [`Self::first_records_of`]: at least that many records, or every one
    /// after the anchor when there are fewer.
    ///
    /// # Errors
    ///
    /// Returns an error when the backend fails or stored bytes cannot be
    /// decoded.
    pub fn records_after(
        &self,
        namespace: NamespaceId,
        database: DatabaseId,
        table: TableId,
        anchor: &RecordId,
        bound: Option<usize>,
    ) -> Result<Vec<(RecordId, Vec<u8>)>> {
        self.table_records(
            namespace,
            database,
            table,
            Walk {
                bound,
                anchor: Some(anchor),
                ..Walk::default()
            },
        )
    }

    /// The first `wanted` live records of one table, in key order.
    ///
    /// # The contract, which is looser than it looks and deliberately so
    ///
    /// Returns **at least** `wanted` records, or every record there is when the
    /// table holds fewer. It may return more, and a caller that asked for a
    /// bound still applies it. An over-return costs a little memory; an
    /// under-return is a **quietly short answer** — the right records, fewer of
    /// them, with nothing raised — so the arithmetic below is deliberately loose
    /// in the safe direction.
    ///
    /// # Why the count is not simply handed to the backend
    ///
    /// Two reasons, and both are the kind that produce a plausible wrong answer
    /// rather than a failure.
    ///
    /// A scan's limit counts **entries**, and a record has as many entries as it
    /// has versions. Asking for `wanted` entries would return fewer than
    /// `wanted` records whenever anything had been updated. So the walk asks for
    /// what it still needs, batch by batch, and counts records rather than rows.
    ///
    /// And this transaction's own uncommitted writes are folded in afterwards,
    /// where a **tombstone** removes a record the walk already counted and
    /// leaves the answer one short. An insert cannot do the same damage — it
    /// only makes the set larger, and the caller's own bound truncates it — so
    /// over-fetching by the number of pending tombstones on this table is
    /// enough, and in the ordinary case, a read outside a write transaction, it
    /// is exactly `wanted`.
    ///
    /// That asymmetry was not obvious and is recorded because it was found the
    /// hard way: a test written to exercise the displacement used one insert and
    /// one delete, which cancelled, and passed with the over-fetch removed.
    ///
    /// # Errors
    ///
    /// Returns an error when the backend fails or stored bytes cannot be
    /// decoded.
    pub fn first_records_of(
        &self,
        namespace: NamespaceId,
        database: DatabaseId,
        table: TableId,
        wanted: usize,
    ) -> Result<Vec<(RecordId, Vec<u8>)>> {
        self.table_records(
            namespace,
            database,
            table,
            Walk {
                bound: Some(wanted),
                ..Walk::default()
            },
        )
    }

    /// The live records of one table whose identity falls in a span.
    ///
    /// A record's key is its table prefix followed by its identity, so a span of
    /// identities is a **span of the keyspace** — the walk starts at `lower` and
    /// stops at `upper`, and the records outside it are never read. That is the
    /// difference between this and a condition over the same field: a condition
    /// reads the table and tests each record, and this one does not read them.
    ///
    /// Both bounds name a **position**, and a position is well defined whether
    /// or not a record sits on it, so neither bound has to exist. `inclusive`
    /// says whether `upper` itself is inside the span; `lower` always is.
    ///
    /// A span whose lower bound sorts above its upper one answers with nothing
    /// rather than failing. It is an empty span, in the way `1..1` is an empty
    /// range, and the alternative is a refusal for a question that has an answer.
    ///
    /// # Errors
    ///
    /// Returns an error when the backend fails or stored bytes cannot be
    /// decoded.
    pub fn records_in_span(
        &self,
        namespace: NamespaceId,
        database: DatabaseId,
        table: TableId,
        lower: &RecordId,
        upper: &RecordId,
        inclusive: bool,
    ) -> Result<Vec<(RecordId, Vec<u8>)>> {
        self.records_of(
            namespace,
            database,
            table,
            Walk {
                span: Some(Span {
                    lower,
                    upper,
                    inclusive,
                }),
                ..Walk::default()
            },
        )
    }

    /// The live records of one table, all of them or the first `bound` of them,
    /// starting past `anchor` when a cursor named one.
    /// The records of a window of the table, after `after`, at most `bound` of
    /// them — one page of a gather (G033, ADR-0083).
    ///
    /// Paged by the caller passing the last identity it received as `after`, so
    /// a page seam neither repeats nor drops a record.
    ///
    /// # Errors
    ///
    /// Whatever reading the table returns.
    pub fn records_between(
        &self,
        namespace: NamespaceId,
        database: DatabaseId,
        table: TableId,
        window: Window<'_>,
        after: Option<&RecordId>,
        bound: usize,
    ) -> Result<Vec<(RecordId, Vec<u8>)>> {
        let mut found = self.table_records(
            namespace,
            database,
            table,
            Walk {
                bound: Some(bound),
                anchor: after,
                from: window.from,
                to: window.to,
                ..Walk::default()
            },
        )?;
        // A walk that knows its bound asks for exactly what it still needs, and
        // a batch may still end past it; the page is the bound.
        found.truncate(bound);
        Ok(found)
    }

    fn table_records(
        &self,
        namespace: NamespaceId,
        database: DatabaseId,
        table: TableId,
        walk: Walk<'_>,
    ) -> Result<Vec<(RecordId, Vec<u8>)>> {
        self.records_of(namespace, database, table, walk)
    }

    /// The walk all four of the readers above share.
    fn records_of(
        &self,
        namespace: NamespaceId,
        database: DatabaseId,
        table: TableId,
        walk: Walk<'_>,
    ) -> Result<Vec<(RecordId, Vec<u8>)>> {
        let Walk {
            bound,
            anchor,
            span,
            from: window_from,
            to: window_to,
            reading,
        } = walk;
        let prefix = RecordKey::table_prefix(namespace, database, table);
        let of_this_table = |address: &RecordAddress| {
            address.namespace == namespace && address.database == database && address.table == table
        };
        // The anchor's own versions are behind the page, not in it, so the walk
        // begins past the last of them rather than at the first.
        // Three ways to start, and they differ by one record. A span opens **at**
        // its lower bound because that bound is inside it; a cursor opens past
        // its anchor's own versions because it has already handed that record
        // over; and a plain walk opens at the table.
        let opening = match (&span, anchor) {
            (Some(span), _) => RecordKey::versions_prefix(namespace, database, table, span.lower),
            (None, Some(anchor)) => after(RecordKey::versions_prefix(
                namespace, database, table, anchor,
            )),
            (None, None) => prefix.clone(),
        };
        // The retention floor raises where the walk opens rather than filtering
        // what it passes, which is the whole reason a series table's identity
        // carries the millisecond: the scan **starts later** and never reads the
        // records it would have discarded. Taken as a maximum, so a span or a
        // cursor already past the floor is not pulled backwards by it.
        // A window's lower end raises the opening exactly as the floor below
        // does: a cursor already past it is not pulled back to it.
        let opening = match window_from {
            Some(lower) => {
                let at = RecordKey::versions_prefix(namespace, database, table, lower);
                if at > opening { at } else { opening }
            }
            None => opening,
        };
        let floor = self.series_floor(namespace, table)?;
        let opening = match &floor {
            Some(floor) => {
                let at = RecordKey::versions_prefix(namespace, database, table, floor);
                if at > opening { at } else { opening }
            }
            None => opening,
        };
        let wanted = bound.map(|bound| {
            let displacing = self
                .writes
                .iter()
                .filter(|(address, value)| {
                    of_this_table(address) && matches!(value, RecordValue::Tombstone)
                })
                .count();
            bound.saturating_add(displacing)
        });

        // Versions of one record are adjacent and sort newest-first, so the
        // first version at or before the snapshot is the visible one and every
        // later entry for that record is an older version to walk past.
        //
        // `resolved` carries across batches for that reason: a record's versions
        // may straddle a boundary, and forgetting which record was just settled
        // would let an older version of it be read as a newer record.
        let mut live: BTreeMap<RecordId, RecordValue> = BTreeMap::new();
        let mut resolved: Option<RecordId> = None;
        // Counted separately from `live.len()`, which includes the tombstones
        // that are about to be filtered out. Stopping on a count that includes
        // them would answer short by however many deleted records the walk
        // happened to pass.
        let mut present = 0_usize;
        let mut from = opening;
        let end = match &span {
            // An exclusive upper bound stops **before** that record's first
            // version; an inclusive one stops after its last.
            Some(span) => {
                let at = RecordKey::versions_prefix(namespace, database, table, span.upper);
                if span.inclusive { after(at) } else { at }
            }
            None => match window_to {
                Some((upper, inclusive)) => {
                    let at = RecordKey::versions_prefix(namespace, database, table, upper);
                    if inclusive { after(at) } else { at }
                }
                None => after(prefix),
            },
        };
        loop {
            let request = ScanRequest {
                keyspace: RecordKey::keyspace(),
                range: KeyRange::between(Key::from(from), Key::from(end.clone())),
                direction: ScanDirection::Forward,
                // The unbounded read stays one scan of everything, unchanged.
                // Only a read that knows what it needs asks for less, and it
                // asks for exactly what it still needs so a small bound costs a
                // small scan rather than a batch-sized one.
                limit: wanted.map(|wanted| {
                    wanted
                        .saturating_sub(present)
                        .clamp(1, RANGE_SCAN_BATCH_ENTRIES)
                }),
            };
            let batch = match reading {
                Reading::Serving => self.store.backend().scan(&request)?,
                Reading::Sweep => self.store.backend().sweep(&request)?,
            };
            let last = batch.last().map(|(key, _)| key.as_slice().to_vec());
            let entries = batch.len();
            for (id, value) in self.settled(batch, &mut resolved)? {
                if matches!(value, RecordValue::Present(_)) {
                    present = present.saturating_add(1);
                }
                live.insert(id, value);
            }
            let Some(wanted) = wanted else {
                break;
            };
            // A batch shorter than asked for is the end of the table; otherwise
            // the walk continues until it has what it came for.
            let Some(last) = last.filter(|_| present < wanted && entries > 0) else {
                break;
            };
            from = resuming_after(last);
        }

        for (address, value) in &self.writes {
            // A pending write is folded in only where the committed walk would
            // have reached it. Without the second test a record written but not
            // yet committed would appear on a page it sorts before, which is the
            // one way a cursor could answer with a record it had already handed
            // the caller.
            // And a span applies to a pending write for the same reason the
            // cursor test does: a record written but not committed still has to
            // be outside a span it is outside of, or a bounded read answers with
            // a record the same read would not have found a moment earlier.
            // A pending write is judged against the floor for the same reason
            // it is judged against the span: a record this transaction wrote
            // below the floor is a record the same read would not have found a
            // moment earlier, and returning it would make the floor a property
            // of who is asking.
            if of_this_table(address)
                && floor.as_ref().is_none_or(|floor| &address.id >= floor)
                && anchor.is_none_or(|anchor| &address.id > anchor)
                && span.as_ref().is_none_or(|span| span.holds(&address.id))
                && window_from.is_none_or(|lower| &address.id >= lower)
                && window_to.is_none_or(|(upper, inclusive)| {
                    if inclusive {
                        &address.id <= upper
                    } else {
                        &address.id < upper
                    }
                })
            {
                live.insert(address.id.clone(), value.clone());
            }
        }

        Ok(live
            .into_iter()
            .filter_map(|(id, value)| match value {
                RecordValue::Present(payload) => Some((id, payload)),
                RecordValue::Tombstone => None,
            })
            .collect())
    }

    /// Which records a batch of raw entries settles, in the order it holds them.
    ///
    /// Versions of one record are adjacent and sort newest-first, so the first
    /// entry at or before the snapshot is the visible one and every later entry
    /// for that record is an older version to walk past. `resolved` is the
    /// caller's because a record's versions may straddle a batch boundary, and
    /// forgetting which record was just settled would let an older version of it
    /// be read as a newer record.
    ///
    /// Asked by both the collecting walk and the streaming one. The two answer
    /// different shapes and must not disagree about which version a reader sees;
    /// two copies of that rule would be two places for it to drift.
    fn settled(
        &self,
        batch: Vec<(Key, tessari_kv::Value)>,
        resolved: &mut Option<RecordId>,
    ) -> Result<Vec<(RecordId, RecordValue)>> {
        let mut taken = Vec::with_capacity(batch.len());
        for (key, value) in batch {
            let decoded = RecordKey::decode(key.as_slice())?;
            if decoded.version > self.snapshot || resolved.as_ref() == Some(&decoded.id) {
                continue;
            }
            *resolved = Some(decoded.id.clone());
            taken.push((
                decoded.id,
                StampedValue::decode(value.as_slice())?.into_visible_at(self.reading_at()),
            ));
        }
        Ok(taken)
    }

    /// One batch of raw entries from a span of the record keyspace.
    ///
    /// Its own method so that a caller whose error type is not this crate's can
    /// still write `?` over the scan: everything fallible about the walk is on
    /// this side of the boundary, and the callback's side converts once.
    fn batch_of(
        &self,
        from: &[u8],
        end: &[u8],
        limit: Option<usize>,
    ) -> Result<Vec<(Key, tessari_kv::Value)>> {
        Ok(self.store.backend().scan(&ScanRequest {
            keyspace: RecordKey::keyspace(),
            range: KeyRange::between(Key::from(from.to_vec()), Key::from(end.to_vec())),
            direction: ScanDirection::Forward,
            limit,
        })?)
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
