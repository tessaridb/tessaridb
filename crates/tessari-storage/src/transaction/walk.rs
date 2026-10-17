//! A walk over records that hands them out a round at a time.
//!
//! # Why a pull and not a callback
//!
//! A caller that stops early is deciding something (testing a condition,
//! feeding a consumer), and both need the transaction between records. A
//! callback would have to borrow the transaction from the walk that is reading
//! through it; a pull lets the caller hold the transaction and lend it to the
//! walk only for the read: `while let Some(round) = walk.next_round(&tx).await?`.
//!
//! # What a round is
//!
//! The records one backend request named, already resolved and in order. A
//! caller that stops after any record of a round has paid for exactly the rounds
//! a walk that could stop mid-round would have read, because a round is read
//! whole either way.

use std::collections::{BTreeMap, VecDeque};

use tessari_constants::RANGE_SCAN_BATCH_ENTRIES;
use tessari_encoding::{RecordValue, SecondaryIndexKey, StoreKey};
use tessari_kv::{Key, KeyRange, ScanDirection, ScanRequest};
use tessari_types::{DatabaseId, NamespaceId, RecordId, TableId};

use super::address::resuming_after;
use super::{RecordAddress, Transaction};
use crate::error::Result;

/// Records of one table or one index read, handed out in rounds.
///
/// Built by [`Transaction::walk_table`] and
/// [`Transaction::walk_records_in_range`]; each round is the next records in
/// the walk's order, with this transaction's own uncommitted writes merged in
/// and its deletes left out.
pub struct RecordWalk {
    walking: Walking,
}

enum Walking {
    Table(TableWalk),
    Equality(EqualityWalk),
    Ids(IdsWalk),
    Done,
}

/// Where a walk reads, so the records it names can be addressed.
#[derive(Clone, Copy)]
pub(super) struct Place {
    pub(super) namespace: NamespaceId,
    pub(super) database: DatabaseId,
    pub(super) table: TableId,
}

impl Place {
    fn address(self, id: RecordId) -> RecordAddress {
        RecordAddress::new(self.namespace, self.database, self.table, id)
    }
}

/// A table in key order, its committed records and this transaction's writes.
pub(super) struct TableWalk {
    /// This transaction's writes to the table, in key order, copied out when
    /// the walk began.
    pub(super) pending: VecDeque<(RecordId, RecordValue)>,
    pub(super) resolved: Option<RecordId>,
    /// Where the committed walk resumes; `None` once it has read to the end.
    pub(super) from: Option<Vec<u8>>,
    pub(super) end: Vec<u8>,
}

/// The entries of one complete value of a secondary index, in record order.
pub(super) struct EqualityWalk {
    pub(super) place: Place,
    /// This transaction's records of the table: a payload, or `None` for a
    /// delete, which hands nothing over.
    pub(super) pending: BTreeMap<RecordId, Option<Vec<u8>>>,
    /// Where the entry walk resumes; `None` once it has read to the end.
    pub(super) from: Option<Vec<u8>>,
    pub(super) end: Vec<u8>,
    pub(super) batch: usize,
}

/// Records already named by identity, fetched a doubling batch at a time.
pub(super) struct IdsWalk {
    pub(super) place: Place,
    pub(super) ids: VecDeque<RecordId>,
    /// This transaction's payloads, which win over what the store holds.
    pub(super) pending: BTreeMap<RecordId, Vec<u8>>,
    pub(super) batch: usize,
}

impl RecordWalk {
    pub(super) const fn table(walk: TableWalk) -> Self {
        Self {
            walking: Walking::Table(walk),
        }
    }

    pub(super) const fn equality(walk: EqualityWalk) -> Self {
        Self {
            walking: Walking::Equality(walk),
        }
    }

    pub(super) const fn ids(walk: IdsWalk) -> Self {
        Self {
            walking: Walking::Ids(walk),
        }
    }

    /// The next records of the walk, or `None` once it is over.
    ///
    /// A round is never empty: a request whose records were all deleted or not
    /// yet visible is followed by the next one before anything is handed out.
    ///
    /// # Errors
    ///
    /// Returns an error when the backend fails or stored bytes cannot be
    /// decoded. The walk is over after an error.
    pub async fn next_round(
        &mut self,
        transaction: &Transaction<'_>,
    ) -> Result<Option<Vec<(RecordId, Vec<u8>)>>> {
        loop {
            let round = match &mut self.walking {
                Walking::Table(walk) => walk.round(transaction).await,
                Walking::Equality(walk) => walk.round(transaction).await,
                Walking::Ids(walk) => walk.round(transaction).await,
                Walking::Done => return Ok(None),
            };
            match round {
                Ok(Some(round)) if round.is_empty() => {}
                Ok(Some(round)) => return Ok(Some(round)),
                Ok(None) => {
                    self.walking = Walking::Done;
                    return Ok(None);
                }
                Err(error) => {
                    self.walking = Walking::Done;
                    return Err(error);
                }
            }
        }
    }
}

impl TableWalk {
    async fn round(
        &mut self,
        transaction: &Transaction<'_>,
    ) -> Result<Option<Vec<(RecordId, Vec<u8>)>>> {
        let Some(from) = self.from.take() else {
            // Whatever the committed walk never reached: records written in
            // this transaction that sort after the last one on disk.
            if self.pending.is_empty() {
                return Ok(None);
            }
            return Ok(Some(present(self.pending.drain(..))));
        };
        // Batched although the walk names no bound: a single unbounded request
        // would read the table before the first record could ask to stop.
        let batch = transaction
            .batch_of(&from, &self.end, Some(RANGE_SCAN_BATCH_ENTRIES))
            .await?;
        let last = batch.last().map(|(key, _)| key.as_slice().to_vec());
        // A batch shorter than the one asked for is the end of the table.
        if batch.len() >= RANGE_SCAN_BATCH_ENTRIES {
            self.from = last.map(resuming_after);
        }
        let mut round = Vec::with_capacity(batch.len());
        for (id, value) in transaction.settled(batch, &mut self.resolved).await? {
            // Pending writes sorting before this record come first, so the
            // records arrive in key order whether they are committed or not.
            while self
                .pending
                .front()
                .is_some_and(|(waiting, _)| waiting < &id)
            {
                if let Some((waiting, RecordValue::Present(payload))) = self.pending.pop_front() {
                    round.push((waiting, payload));
                }
            }
            // A record this transaction has written is answered from the write
            // and not from the committed version beneath it, including when the
            // write is a tombstone, which removes it.
            let value = if self
                .pending
                .front()
                .is_some_and(|(waiting, _)| waiting == &id)
            {
                self.pending
                    .pop_front()
                    .map_or(value, |(_, written)| written)
            } else {
                value
            };
            if let RecordValue::Present(payload) = value {
                round.push((id, payload));
            }
        }
        Ok(Some(round))
    }
}

impl EqualityWalk {
    async fn round(
        &mut self,
        transaction: &Transaction<'_>,
    ) -> Result<Option<Vec<(RecordId, Vec<u8>)>>> {
        let Some(from) = self.from.take() else {
            return Ok(None);
        };
        let request = ScanRequest {
            keyspace: SecondaryIndexKey::keyspace(),
            range: KeyRange::between(Key::from(from), Key::from(self.end.clone())),
            direction: ScanDirection::Forward,
            limit: Some(self.batch),
        };
        let entries = transaction.store.backend().scan(&request).await?;
        let full = entries.len() >= self.batch;
        let mut named = Vec::with_capacity(entries.len());
        for (key, _) in &entries {
            named.push(SecondaryIndexKey::decode(key.as_slice())?.id);
        }
        // This round answers every identity up to the last entry it read, or,
        // once the entries are exhausted, every pending one left.
        let through = if full { named.last().cloned() } else { None };
        let mut round: BTreeMap<RecordId, Option<Option<Vec<u8>>>> =
            named.into_iter().map(|id| (id, None)).collect();
        let waiting: Vec<RecordId> = self
            .pending
            .keys()
            .filter(|id| through.as_ref().is_none_or(|last| *id <= last))
            .cloned()
            .collect();
        for id in waiting {
            if let Some(payload) = self.pending.remove(&id) {
                round.insert(id, Some(payload));
            }
        }
        // The stored records this round names, resolved together.
        let stored: Vec<RecordAddress> = round
            .iter()
            .filter(|(_, written)| written.is_none())
            .map(|(id, _)| self.place.address(id.clone()))
            .collect();
        let mut fetched = stored
            .iter()
            .map(|at| at.id.clone())
            .zip(transaction.get_each(&stored).await?)
            .collect::<BTreeMap<_, _>>();
        let mut handed = Vec::with_capacity(round.len());
        for (id, written) in round {
            let payload = match written {
                Some(payload) => payload,
                None => fetched.remove(&id).flatten(),
            };
            if let Some(payload) = payload {
                handed.push((id, payload));
            }
        }
        if let Some((last, _)) = entries.last().filter(|_| full) {
            self.from = Some(resuming_after(last.as_slice().to_vec()));
            self.batch = self.batch.saturating_mul(2).min(RANGE_SCAN_BATCH_ENTRIES);
        }
        Ok(Some(handed))
    }
}

impl IdsWalk {
    async fn round(
        &mut self,
        transaction: &Transaction<'_>,
    ) -> Result<Option<Vec<(RecordId, Vec<u8>)>>> {
        if self.ids.is_empty() {
            return Ok(None);
        }
        let take = self.ids.len().min(self.batch);
        let addresses: Vec<RecordAddress> = self
            .ids
            .drain(..take)
            .map(|id| self.place.address(id))
            .collect();
        let payloads = transaction.get_each(&addresses).await?;
        let mut handed = Vec::with_capacity(addresses.len());
        for (address, stored) in addresses.into_iter().zip(payloads) {
            if let Some(payload) = self.pending.get(&address.id).cloned().or(stored) {
                handed.push((address.id, payload));
            }
        }
        self.batch = self.batch.saturating_mul(2).min(RANGE_SCAN_BATCH_ENTRIES);
        Ok(Some(handed))
    }
}

/// The payloads of the pending writes that are not deletes, in order.
fn present(pending: impl Iterator<Item = (RecordId, RecordValue)>) -> Vec<(RecordId, Vec<u8>)> {
    pending
        .filter_map(|(id, value)| match value {
            RecordValue::Present(payload) => Some((id, payload)),
            RecordValue::Tombstone => None,
        })
        .collect()
}
