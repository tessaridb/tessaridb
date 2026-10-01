//! Bringing a follower that fell behind the log back to its leader's state
//! (ADR-0094 D3, amendment 4').
//!
//! The copy itself is the snapshot restore's: chunks of the leader's state,
//! applied by [`Store::restore_state_chunk`] over whatever this node holds. What
//! a copy cannot do on its own is remove a record the leader no longer has,
//! because a state is the records that exist. This module does that half: every
//! live record in the reach that the copy did not rewrite is gone at the leader,
//! so it is written as a tombstone through the same validate-and-derive path.

use std::ops::Bound;

use tessari_encoding::{
    LogRecord, Mutation, RecordKey, RecordValue, StampedValue, StoreKey, StoreValue,
};
use tessari_kv::{Key, KeyRange, ScanDirection, ScanRequest};
use tessari_types::{Reach, Sequence};

use super::Store;
use crate::catalog::system;
use crate::error::Result;

/// How many stored keys one page of the walk reads, and how many tombstones
/// one applied chunk carries.
const PAGE: usize = 1_000;

impl Store {
    /// Remove every live record inside `within` whose newest version is at or
    /// below `before` — the version this node stood at before a copy rewrote
    /// what the leader still holds — and answer how many were removed.
    ///
    /// Data before catalog: a record of a table the leader dropped is removed
    /// while its definition still validates it, and the definition after.
    ///
    /// # Errors
    ///
    /// Returns an error when the store cannot be read, a stored value cannot be
    /// decoded, or a tombstone is refused by the catalog.
    pub fn sweep_unreplaced(&self, within: Reach, before: Sequence) -> Result<u64> {
        let mut catalog: Vec<Mutation> = Vec::new();
        let mut data: Vec<Mutation> = Vec::new();
        let mut removed = 0_u64;
        let mut after: Option<Key> = None;
        let mut decided: Option<Vec<u8>> = None;
        loop {
            let request = ScanRequest {
                keyspace: RecordKey::keyspace(),
                range: KeyRange::from_bounds(
                    after.clone().map_or(Bound::Unbounded, Bound::Excluded),
                    Bound::Unbounded,
                ),
                direction: ScanDirection::Forward,
                limit: Some(PAGE),
            };
            let page = self.backend().sweep(&request)?;
            let last = page.len() < PAGE;
            for (key, value) in page {
                after = Some(key.clone());
                let stored = RecordKey::decode(key.as_slice())?;
                let prefix = RecordKey::versions_prefix(
                    stored.namespace,
                    stored.database,
                    stored.table,
                    &stored.id,
                );
                // Versions sort newest-first: the first one seen is the record's
                // newest, and the rest of its versions are older history.
                if decided.as_deref() == Some(prefix.as_slice()) {
                    continue;
                }
                decided = Some(prefix);
                if stored.version > before || crate::state::derived(&stored) {
                    continue;
                }
                if matches!(
                    StampedValue::decode(value.as_slice())?.value(),
                    RecordValue::Tombstone
                ) {
                    continue;
                }
                let shard = crate::catalog::Catalog::new(&mut self.begin()?)
                    .table(stored.table)?
                    .and_then(|definition| definition.shards)
                    .map(|map| map.shard_of(&stored.id));
                let mutation = Mutation {
                    namespace: stored.namespace,
                    database: stored.database,
                    table: stored.table,
                    id: stored.id,
                    shard,
                    value: StampedValue::new(RecordValue::Tombstone),
                };
                if !crate::catalog::carried_to(&mutation)?.reaches(within) {
                    continue;
                }
                if mutation.namespace == system::SYSTEM_NAMESPACE {
                    catalog.push(mutation);
                } else {
                    data.push(mutation);
                    if data.len() >= PAGE {
                        removed = removed.saturating_add(self.tombstone(&mut data)?);
                    }
                }
            }
            if last {
                break;
            }
        }
        removed = removed.saturating_add(self.tombstone(&mut data)?);
        removed = removed.saturating_add(self.tombstone(&mut catalog)?);
        Ok(removed)
    }

    /// Apply a held run of tombstones as one chunk, and empty the run.
    fn tombstone(&self, held: &mut Vec<Mutation>) -> Result<u64> {
        if held.is_empty() {
            return Ok(0);
        }
        let count = u64::try_from(held.len()).unwrap_or(u64::MAX);
        self.restore_state_chunk(&LogRecord::new(std::mem::take(held)))?;
        Ok(count)
    }
}
