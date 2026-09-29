//! Reads over a span of record ids, from the first record or after one.

use super::{Span, Transaction, Walk, Window};
use crate::error::Result;
use tessari_types::{DatabaseId, NamespaceId, RecordId, TableId};

impl Transaction<'_> {
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
}
