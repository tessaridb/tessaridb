//! Reading changes and history back out of the logs.

use std::ops::Bound;

use tessari_constants::HISTORY_SCAN_RECORDS;
use tessari_encoding::{LogId, LogKey, LogRecord, StoreKey, StoreValue};
use tessari_kv::{KeyRange, ScanDirection, ScanRequest};
use tessari_types::Sequence;

use crate::catalog::Reach;
use crate::error::{Error, Result};
use crate::feed::Changes;
use crate::feed::{History, Subject};

use super::Store;

impl Store {
    /// Read log records from `from` onward, oldest first.
    ///
    /// `limit` bounds the read because a log is unbounded by nature and a caller
    /// that asks for "the rest of it" is asking for however much has accumulated
    /// since it last looked.
    ///
    /// # Errors
    ///
    /// Returns an error when the backend fails or a stored record cannot be
    /// decoded.
    /// The record changes from `from` onward, oldest first.
    ///
    /// A projection of [`Store::log_records`] and nothing more: the feed holds no
    /// state, cannot disagree with what was committed, and is identical on a
    /// replica reading the same log. Catalog changes are not in it — a
    /// subscriber watching `users` did not ask for the rows that describe
    /// `users` — and a change says what a record *became* rather than whether it
    /// is new; both are explained in [`crate::feed`].
    ///
    /// `limit` bounds the **log records** read, not the changes produced, so one
    /// commit is never returned half-way: a subscriber applies a commit as the
    /// unit it was written as. For the same reason the answer carries the
    /// position to resume from — a commit that only touched the catalog yields
    /// no changes, and a reader given only a list could not tell that from
    /// "nothing has happened" and would ask for the same records forever.
    ///
    /// # Errors
    ///
    /// Returns an error when the backend fails, or when a record or a payload
    /// cannot be decoded. A payload that cannot be decoded is corruption rather
    /// than a change to skip.
    pub fn changes_since(&self, log: LogId, from: Sequence, limit: usize) -> Result<Changes> {
        let records = self.log_records(log, from, limit)?;
        let next = records.last().map_or(from, |(sequence, _)| {
            Sequence::new(sequence.get().saturating_add(1))
        });
        let mut changes = Vec::new();
        for (sequence, record) in records {
            changes.extend(crate::feed::changes_in(sequence, &record)?);
        }
        Ok(Changes { changes, next })
    }

    /// One object's history, newest first, out of the log that already holds it.
    ///
    /// The store writes no event stream and needs none: every commit is a log
    /// record carrying the address of everything it changed, so a history is a
    /// projection of the log exactly as the change feed is. Building a second,
    /// parallel event keyspace would double-write the same bytes on the commit
    /// path and add a second unbounded region to a store that already has one.
    ///
    /// # The walk is bounded, not the answer
    ///
    /// This reads **backwards** from the newest record, which is what makes a
    /// recently-written object cheap. It is also what makes a long-untouched one
    /// expensive: the walk finds nothing the whole way down, and no caller can
    /// tell which case it is in before asking. So the number of log records read
    /// is capped by [`HISTORY_SCAN_RECORDS`] and a read that hits the cap says
    /// so — a screen that shows five events and implies they are all of them is
    /// worse than one that shows five and says there may be more.
    ///
    /// `complete` is true only when the walk reached the beginning of the log.
    ///
    /// # Errors
    ///
    /// Returns an error when the backend fails or a stored record cannot be
    /// decoded. A record that cannot be decoded is corruption rather than an
    /// event to skip, for the reason [`crate::feed::changes_in`] gives.
    pub fn history_of(&self, log: LogId, subject: &Subject, limit: usize) -> Result<History> {
        let records = self.log_records_newest_first(log, HISTORY_SCAN_RECORDS)?;
        let walked = records.len();
        let mut events = Vec::new();
        for (sequence, record) in records {
            for change in crate::feed::changes_in(sequence, &record)? {
                if subject.covers(&change) {
                    events.push(change);
                }
            }
            if events.len() >= limit {
                break;
            }
        }
        events.truncate(limit);
        Ok(History {
            events,
            // Reaching the cap means there may be older records below. Walking
            // fewer than the cap USED to mean the log ended first, and stopped
            // meaning it the moment a log could be pruned: a walk that runs out
            // early on a pruned log has reached the horizon rather than the
            // beginning, and reporting that as complete would present a
            // truncated history as the whole of one. Both conditions, therefore.
            complete: walked < HISTORY_SCAN_RECORDS && self.log_start(log)?.get() <= 1,
            walked,
        })
    }

    /// One home's newest log records, newest first.
    ///
    /// The reverse twin of [`Self::log_records`]. `limit` is not a convenience
    /// here either — the whole point of reading from the tail is to do bounded
    /// work, and an unbounded reverse scan would simply be the forward one with
    /// its cost hidden.
    ///
    /// # Errors
    ///
    /// Returns an error when the backend fails or a stored record cannot be
    /// decoded.
    pub fn log_records_newest_first(
        &self,
        log: LogId,
        limit: usize,
    ) -> Result<Vec<(Sequence, LogRecord)>> {
        let prefix = LogKey::prefix_for(log);
        let request = ScanRequest {
            keyspace: LogKey::keyspace(),
            range: KeyRange::prefix(&prefix),
            direction: ScanDirection::Reverse,
            limit: Some(limit),
        };
        self.backend
            .scan(&request)?
            .into_iter()
            .map(|(key, value)| {
                let sequence = LogKey::decode(key.as_slice())?.sequence;
                let record = LogRecord::decode(value.as_slice())?;
                Ok((sequence, record))
            })
            .collect()
    }

    /// Read one home's log records from `from` onward, oldest first.
    ///
    /// `limit` bounds the read because a log is unbounded by nature and a caller
    /// that asks for "the rest of it" is asking for however much has accumulated
    /// since it last looked.
    ///
    /// **One home per call, and that is the signature the cursor forces**
    /// (Q-621). `from` is a position, and after the log became per-range there
    /// is no space a single position counts in across homes: a caller reading
    /// the chain from the store down to its own reach holds one position per log
    /// and asks once for each.
    ///
    /// # Errors
    ///
    /// Returns an error when the backend fails or a stored record cannot be
    /// decoded.
    pub fn log_records(
        &self,
        log: LogId,
        from: Sequence,
        limit: usize,
    ) -> Result<Vec<(Sequence, LogRecord)>> {
        // Refused rather than answered short, and this is the one place it can
        // be: every reader of the log arrives here, and *short* already means
        // something else on this path — it is how a follower is told it is
        // level. A pruned span answered as a short read would tell a follower it
        // had caught up while the records it is missing no longer exist, and
        // nothing anywhere would be in an error state (`Error::BelowLogStart`).
        let start = self.log_start(log)?;
        if start > from {
            return Err(Error::BelowLogStart {
                asked: from.get(),
                start: start.get(),
            });
        }
        let prefix = LogKey::prefix_for(log);
        let bounds = KeyRange::prefix(&prefix);
        let request = ScanRequest {
            keyspace: LogKey::keyspace(),
            range: KeyRange::from_bounds(
                Bound::Included(LogKey::new(log, from).encode()),
                bounds.end().clone(),
            ),
            direction: ScanDirection::Forward,
            limit: Some(limit),
        };
        self.backend
            .scan(&request)?
            .into_iter()
            .map(|(key, value)| {
                let sequence = LogKey::decode(key.as_slice())?.sequence;
                let record = LogRecord::decode(value.as_slice())?;
                Ok((sequence, record))
            })
            .collect()
    }

    /// Log records for a subscriber, carrying only what its reach reaches.
    ///
    /// # Every sequence arrives, and that is the whole of the design
    ///
    /// The leader does not skip a record it filtered to nothing — it delivers it
    /// empty. A follower's position check compares the epoch of the record
    /// **before** the one it is offered (ADR-0059), so a record that simply
    /// vanished from the numbering would read as a parted history and refuse the
    /// stream. Delivering it empty costs a frame and keeps the arithmetic the
    /// gap rule already does: [`Self::apply_record`] advances `committed_tail`
    /// over a record with no mutations exactly as it does over a full one.
    ///
    /// The cost is stated rather than discovered: a selective follower's stream
    /// is O(all commits) in **frames** while being O(its own commits) in bytes.
    /// Bounded by commits rather than by data.
    ///
    /// # The filter is on the leader, deliberately
    ///
    /// A follower could be sent everything and asked to keep what it is entitled
    /// to. That is a confidentiality model in which the party being restricted
    /// is the one applying the restriction, and it is the arrangement this store
    /// refuses everywhere else. The epoch is preserved across the rebuild
    /// because it identifies the leadership that wrote the commit, not its
    /// contents.
    ///
    /// # Errors
    ///
    /// Returns whatever [`Self::log_records`] returns, and
    /// [`Error::CatalogMalformed`] when a catalog record in the log is present
    /// and cannot be decoded.
    pub fn log_records_within(
        &self,
        subscription: Reach,
        log: LogId,
        from: Sequence,
        limit: usize,
    ) -> Result<Vec<(Sequence, LogRecord)>> {
        if subscription == Reach::Store {
            // Not an optimisation with a caveat — a statement. A store-reach
            // subscription receives the log unchanged, so rebuilding every
            // record to arrive at the same bytes would cost a clone per mutation
            // on the path every follower that exists today takes, to prove
            // something the type already says.
            return self.log_records(log, from, limit);
        }
        let mut carried = Vec::new();
        for (sequence, record) in self.log_records(log, from, limit)? {
            let mut kept = Vec::new();
            for mutation in record.mutations() {
                if crate::catalog::carried_to(mutation)?.reaches(subscription) {
                    kept.push(mutation.clone());
                }
            }
            // An emptied record is still a commit of its writer, and a
            // follower merging logs by order needs to know where it stood even
            // when nothing in it was carried (ADR-0084).
            let mut rebuilt = LogRecord::at(record.epoch(), kept);
            if let Some(order) = record.order() {
                rebuilt.set_order(order);
            }
            carried.push((sequence, rebuilt));
        }
        Ok(carried)
    }
}
