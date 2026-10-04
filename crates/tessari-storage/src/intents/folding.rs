//! Settled transactions across leaders folded away with reclamation (ADR-0112
//! D6a, Q-922).
//!
//! A reader decides a version that names a transaction across leaders from the
//! markers of where each part landed here: an absent marker means *not landed*,
//! so a marker cannot simply be deleted. Once no live snapshot is below every
//! marker of a settled transaction, though, a version that names nothing answers
//! every live reader the same — and a historical `VERSION` read below would not,
//! so the reclaim floor rises past the markers in the same batch, refusing those
//! reads rather than answering them differently. Folding exact history away is
//! what reclamation is; this is its share for transactions across leaders, and
//! markers are then bounded exactly as version history is.
//!
//! Settled here means no intent of it stands, its record (if held) has decided,
//! and every part of a range this node holds has landed. A transaction with
//! markers but neither an intent nor a resolved version left aborted here:
//! nothing reads its markers any more.

use std::collections::{BTreeMap, BTreeSet};

use tessari_encoding::{
    AcrossPartKey, RecordKey, ResolvedOfKey, StampedValue, StoreKey, StoreValue, TransactionId,
};
use tessari_kv::{Key, KeyRange, ScanDirection, ScanRequest, WriteBatch};
use tessari_types::{Reach, Sequence};

use crate::error::Result;
use crate::store::Store;

impl Store {
    /// Fold every settled transaction across leaders whose newest marker is at
    /// or below `floor` — the oldest snapshot a live reader holds — raising the
    /// reclaim floor to it with each. Answers how many were folded.
    ///
    /// # Errors
    ///
    /// Whatever the backend or the codec returns.
    pub(crate) fn fold_settled_across(&self, floor: Sequence) -> Result<usize> {
        let mut markers: BTreeMap<TransactionId, Vec<(Key, Reach, Sequence)>> = BTreeMap::new();
        for (key, value) in self.scan_kind(&[AcrossPartKey::KIND.tag()])? {
            let part = AcrossPartKey::decode(key.as_slice())?;
            let landed = Sequence::decode(value.as_slice())?;
            markers
                .entry(part.transaction)
                .or_default()
                .push((key, part.range, landed));
        }
        let mut folded = 0_usize;
        for (transaction, parts) in markers {
            if parts.iter().any(|(_, _, landed)| *landed > floor)
                || self.holds_intents_of(transaction)?
                || self
                    .transaction_record(transaction)?
                    .is_some_and(|record| !record.decision.is_decided())
            {
                continue;
            }
            let Some(mut batch) = self.settle_versions(transaction, &parts)? else {
                continue;
            };
            for (key, _, _) in parts {
                batch = batch.delete(AcrossPartKey::keyspace(), key);
            }
            // In the same batch, as reclamation raises it with its removals: a
            // read below the floor could still tell the provenance was there.
            let raised = self.reclaim_floor()?.max(floor);
            batch = batch.put(
                tessari_encoding::ReclaimFloorKey::keyspace(),
                tessari_encoding::ReclaimFloorKey.encode(),
                raised.encode(),
            );
            self.backend().apply(batch)?;
            folded = folded.saturating_add(1);
        }
        Ok(folded)
    }

    /// The batch rewriting `transaction`'s resolved versions without their
    /// provenance, or `None` while a part of a range this node holds has not
    /// landed here — a reader of one of them still needs every marker.
    fn settle_versions(
        &self,
        transaction: TransactionId,
        parts: &[(Key, Reach, Sequence)],
    ) -> Result<Option<WriteBatch>> {
        let landed: BTreeSet<Reach> = parts.iter().map(|(_, range, _)| *range).collect();
        let served = self.served();
        let mut batch = WriteBatch::new();
        for (key, value) in self.scan_kind(&ResolvedOfKey::prefix_of(transaction))? {
            let held = ResolvedOfKey::decode(key.as_slice())?;
            let version = Sequence::decode(value.as_slice())?;
            batch = batch.delete(ResolvedOfKey::keyspace(), key);
            let address =
                RecordKey::new(held.namespace, held.database, held.table, held.id, version)
                    .encode();
            // Reclaimed already: nothing left to rewrite.
            let Some(stored) = self.backend().get(RecordKey::keyspace(), &address)? else {
                continue;
            };
            let stored = StampedValue::decode(stored.as_slice())?;
            let missing = stored.provenance().is_some_and(|provenance| {
                provenance.participants.iter().any(|participant| {
                    served.is_none_or(|over| over.contains(participant.range))
                        && !landed.contains(&participant.range)
                })
            });
            if missing {
                return Ok(None);
            }
            batch = batch.put(RecordKey::keyspace(), address, stored.settled().encode());
        }
        Ok(Some(batch))
    }

    /// Every entry under `prefix`, in key order.
    fn scan_kind(&self, prefix: &[u8]) -> Result<Vec<(Key, tessari_kv::Value)>> {
        Ok(self.backend().scan(&ScanRequest {
            keyspace: AcrossPartKey::keyspace(),
            range: KeyRange::prefix(prefix),
            direction: ScanDirection::Forward,
            limit: None,
        })?)
    }
}
