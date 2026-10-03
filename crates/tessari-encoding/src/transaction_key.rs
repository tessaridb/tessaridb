//! The key of one transaction across leaders' record (ADR-0112).

use tessari_kv::Key;
use tessari_types::{DatabaseId, NamespaceId, RecordId, Sequence, TableId};

use crate::error::Result;
use crate::keys::StoreKey;
use crate::kind::KeyKind;
use crate::order::{KeyReader, KeyWriter};
use crate::value::{TRANSACTION_ID_LEN, TransactionId, TransactionRecord};

/// Addresses the record of one transaction across leaders.
///
/// ```text
/// <0x50> <transaction:16>
/// ```
///
/// Fixed width and read only by point lookup: a reader meeting an intent asks
/// for its transaction's record by id, and nothing walks these keys on a read
/// path. The record is node-local state derived from the coordinator range's
/// log, as a catalog row is — the log record is the replicated truth.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TransactionRecordKey {
    /// The transaction.
    pub transaction: TransactionId,
}

impl StoreKey for TransactionRecordKey {
    type Value = TransactionRecord;

    const KIND: KeyKind = KeyKind::TransactionRecord;

    fn encode(&self) -> Key {
        let mut writer = KeyWriter::with_capacity(TRANSACTION_ID_LEN.saturating_add(1));
        writer
            .put_u8(Self::KIND.tag())
            .put_fixed(&self.transaction.bytes());
        Key::from(writer.finish())
    }

    fn decode(bytes: &[u8]) -> Result<Self> {
        let mut reader = KeyReader::new(Self::KIND, bytes);
        reader.expect_kind()?;
        let transaction = TransactionId::new(reader.take_fixed::<TRANSACTION_ID_LEN>()?);
        reader.finish()?;
        Ok(Self { transaction })
    }
}

/// One intent a transaction across leaders holds on this node (ADR-0112 D7).
///
/// ```text
/// <0x51> <transaction:16> <namespace:u32> <database:u32> <table:u32> <record-id>
/// ```
///
/// The transaction leads, so every intent of one transaction is one prefix,
/// and a walk of the kind lists the transactions with intents standing — which
/// is what lets a participant resolve its own intents after the coordinator
/// that knew their addresses is gone. Written in the batch that lands the
/// intent and deleted in the batch that resolves it, so it is never out of step
/// with the intents themselves. The value is the intent's version.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IntentOfKey {
    /// The transaction.
    pub transaction: TransactionId,
    /// The record's namespace.
    pub namespace: NamespaceId,
    /// Its database.
    pub database: DatabaseId,
    /// Its table.
    pub table: TableId,
    /// Its identity.
    pub id: RecordId,
}

impl IntentOfKey {
    /// The prefix every intent of `transaction` shares.
    #[must_use]
    pub fn prefix_of(transaction: TransactionId) -> Vec<u8> {
        let mut writer = KeyWriter::with_capacity(TRANSACTION_ID_LEN.saturating_add(1));
        writer
            .put_u8(KeyKind::IntentOf.tag())
            .put_fixed(&transaction.bytes());
        writer.finish()
    }

    /// The prefix every key of this kind shares.
    #[must_use]
    pub fn prefix() -> Vec<u8> {
        vec![KeyKind::IntentOf.tag()]
    }
}

impl StoreKey for IntentOfKey {
    type Value = Sequence;

    const KIND: KeyKind = KeyKind::IntentOf;

    fn encode(&self) -> Key {
        let mut writer = KeyWriter::with_capacity(48);
        writer
            .put_u8(Self::KIND.tag())
            .put_fixed(&self.transaction.bytes())
            .put_u32(self.namespace.get())
            .put_u32(self.database.get())
            .put_u32(self.table.get());
        crate::record_id::put(&mut writer, &self.id);
        Key::from(writer.finish())
    }

    fn decode(bytes: &[u8]) -> Result<Self> {
        let mut reader = KeyReader::new(Self::KIND, bytes);
        reader.expect_kind()?;
        let transaction = TransactionId::new(reader.take_fixed::<TRANSACTION_ID_LEN>()?);
        let namespace = NamespaceId::new(reader.take_u32()?);
        let database = DatabaseId::new(reader.take_u32()?);
        let table = TableId::new(reader.take_u32()?);
        let id = crate::record_id::take(&mut reader)?;
        reader.finish()?;
        Ok(Self {
            transaction,
            namespace,
            database,
            table,
            id,
        })
    }
}

#[cfg(test)]
mod tests;
