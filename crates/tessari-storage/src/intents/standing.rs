//! What this node holds of transactions across leaders: their records, their
//! intents, and the transactions those intents belong to (ADR-0112 D7, D12).

use tessari_encoding::{
    IntentOfKey, RecordKey, StampedValue, StoreKey, StoreValue, TransactionRecord,
    TransactionRecordKey,
};
use tessari_types::Sequence;

use crate::error::Result;
use crate::store::Store;

impl Store {
    /// The record of one transaction across leaders as this node holds it.
    ///
    /// # Errors
    ///
    /// Whatever the backend or the codec returns.
    pub fn transaction_record(
        &self,
        transaction: tessari_encoding::TransactionId,
    ) -> Result<Option<TransactionRecord>> {
        let key = TransactionRecordKey { transaction };
        Ok(self
            .backend()
            .get(TransactionRecordKey::keyspace(), &key.encode())?
            .map(|value| TransactionRecord::decode(value.as_slice()))
            .transpose()?)
    }

    /// Where `transaction`'s part in `range` landed on this node, if it did —
    /// what status recovery asks a participant's leader (ADR-0112 D14c).
    ///
    /// # Errors
    ///
    /// Whatever the backend or the codec returns.
    pub fn part_landed(
        &self,
        transaction: tessari_encoding::TransactionId,
        range: tessari_types::Reach,
    ) -> Result<Option<Sequence>> {
        let key = tessari_encoding::AcrossPartKey { transaction, range };
        Ok(self
            .backend()
            .get(tessari_encoding::AcrossPartKey::keyspace(), &key.encode())?
            .map(|value| Sequence::decode(value.as_slice()))
            .transpose()?)
    }

    /// Every record `transaction` holds an intent on here, from its index.
    ///
    /// # Errors
    ///
    /// Whatever the backend or the codec returns.
    pub(crate) fn intents_of(
        &self,
        transaction: tessari_encoding::TransactionId,
    ) -> Result<Vec<crate::transaction::RecordAddress>> {
        let found = self.backend().scan(&tessari_kv::ScanRequest {
            keyspace: IntentOfKey::keyspace(),
            range: tessari_kv::KeyRange::prefix(&IntentOfKey::prefix_of(transaction)),
            direction: tessari_kv::ScanDirection::Forward,
            limit: None,
        })?;
        found
            .into_iter()
            .map(|(key, _)| {
                let held = IntentOfKey::decode(key.as_slice())?;
                Ok(crate::transaction::RecordAddress::new(
                    held.namespace,
                    held.database,
                    held.table,
                    held.id,
                ))
            })
            .collect()
    }

    /// Every transaction across leaders with an intent standing here, and the
    /// range its record lives in — read off one of its intents, which every
    /// prepare stamped with it.
    ///
    /// # Errors
    ///
    /// Whatever the backend or the codec returns.
    pub fn standing_across(
        &self,
    ) -> Result<Vec<(tessari_encoding::TransactionId, tessari_types::Reach)>> {
        let found = self.backend().scan(&tessari_kv::ScanRequest {
            keyspace: IntentOfKey::keyspace(),
            range: tessari_kv::KeyRange::prefix(&IntentOfKey::prefix()),
            direction: tessari_kv::ScanDirection::Forward,
            limit: None,
        })?;
        let mut standing: Vec<(tessari_encoding::TransactionId, tessari_types::Reach)> = Vec::new();
        for (key, version) in found {
            let held = IntentOfKey::decode(key.as_slice())?;
            if standing
                .last()
                .is_some_and(|(seen, _)| *seen == held.transaction)
            {
                continue;
            }
            let intent = RecordKey::new(
                held.namespace,
                held.database,
                held.table,
                held.id,
                Sequence::decode(version.as_slice())?,
            );
            let Some(stored) = self
                .backend()
                .get(RecordKey::keyspace(), &intent.encode())?
            else {
                continue;
            };
            if let Some(provenance) = StampedValue::decode(stored.as_slice())?.provenance() {
                standing.push((held.transaction, provenance.coordinator));
            }
        }
        Ok(standing)
    }

    /// How many parts of transactions across leaders are barred on this node
    /// (ADR-0112 D14c), each until this node's log is pruned past the record
    /// that barred it (ADR-0119).
    ///
    /// # Errors
    ///
    /// Whatever the backend returns.
    pub fn bars_across(&self) -> Result<usize> {
        Ok(self
            .backend()
            .scan(&tessari_kv::ScanRequest {
                keyspace: tessari_encoding::AcrossBarredKey::keyspace(),
                range: tessari_kv::KeyRange::prefix(&[
                    tessari_encoding::KeyKind::AcrossBarred.tag()
                ]),
                direction: tessari_kv::ScanDirection::Forward,
                limit: None,
            })?
            .len())
    }

    /// Whether `transaction` holds an intent here.
    ///
    /// # Errors
    ///
    /// Whatever the backend returns.
    pub fn holds_intents_of(&self, transaction: tessari_encoding::TransactionId) -> Result<bool> {
        let found = self.backend().scan(&tessari_kv::ScanRequest {
            keyspace: IntentOfKey::keyspace(),
            range: tessari_kv::KeyRange::prefix(&IntentOfKey::prefix_of(transaction)),
            direction: tessari_kv::ScanDirection::Forward,
            limit: Some(1),
        })?;
        Ok(!found.is_empty())
    }

    /// Every transaction record this node holds that has decided, for the
    /// pass that forgets them (ADR-0112 D12).
    ///
    /// # Errors
    ///
    /// Whatever the backend or the codec returns.
    pub fn decided_across(
        &self,
    ) -> Result<Vec<(tessari_encoding::TransactionId, TransactionRecord)>> {
        Ok(self
            .records_across()?
            .into_iter()
            .filter(|(_, record)| record.decision.is_decided())
            .collect())
    }

    /// Every transaction record this node holds that has not decided yet —
    /// `PENDING`, or `STAGING` awaiting status recovery (ADR-0112 D14c).
    ///
    /// # Errors
    ///
    /// Whatever the backend or the codec returns.
    pub fn pending_across(
        &self,
    ) -> Result<Vec<(tessari_encoding::TransactionId, TransactionRecord)>> {
        Ok(self
            .records_across()?
            .into_iter()
            .filter(|(_, record)| !record.decision.is_decided())
            .collect())
    }

    /// Every transaction record this node holds.
    fn records_across(&self) -> Result<Vec<(tessari_encoding::TransactionId, TransactionRecord)>> {
        let found = self.backend().scan(&tessari_kv::ScanRequest {
            keyspace: TransactionRecordKey::keyspace(),
            range: tessari_kv::KeyRange::prefix(&[
                tessari_encoding::KeyKind::TransactionRecord.tag()
            ]),
            direction: tessari_kv::ScanDirection::Forward,
            limit: None,
        })?;
        found
            .into_iter()
            .map(|(key, value)| {
                Ok((
                    TransactionRecordKey::decode(key.as_slice())?.transaction,
                    TransactionRecord::decode(value.as_slice())?,
                ))
            })
            .collect()
    }
}
