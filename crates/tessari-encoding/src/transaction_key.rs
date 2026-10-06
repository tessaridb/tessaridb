//! The key of one transaction across leaders' record (ADR-0112).

use tessari_kv::Key;
use tessari_types::{DatabaseId, NamespaceId, Reach, RecordId, Sequence, TableId};

use crate::error::Result;
use crate::keys::{StoreKey, put_reach, take_reach};
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

/// One version a committed transaction across leaders resolved on this node
/// (ADR-0112, Q-922): the same layout as [`IntentOfKey`] under its own kind,
/// written in the batch that lands the version and deleted in the batch that
/// folds the transaction's provenance away. The value is the version.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedOfKey {
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

impl ResolvedOfKey {
    /// The prefix every resolved version of `transaction` shares.
    #[must_use]
    pub fn prefix_of(transaction: TransactionId) -> Vec<u8> {
        let mut writer = KeyWriter::with_capacity(TRANSACTION_ID_LEN.saturating_add(1));
        writer
            .put_u8(KeyKind::ResolvedOf.tag())
            .put_fixed(&transaction.bytes());
        writer.finish()
    }
}

impl StoreKey for ResolvedOfKey {
    type Value = Sequence;

    const KIND: KeyKind = KeyKind::ResolvedOf;

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

/// Where one transaction across leaders' part in one range landed on this node
/// (ADR-0112 D6a).
///
/// ```text
/// <0x52> <transaction:16> <range reach>
/// ```
///
/// Written in the batch that applies the range's prepare, with the local
/// version that batch commits at as its value — so a reader whose snapshot is
/// at or past that version holds every intent the prepare wrote, and one whose
/// snapshot is before it holds none. A reader of one of the transaction's
/// versions asks this key for each other range the transaction wrote and this
/// node holds; one missing or past its snapshot makes the whole transaction
/// invisible to it. Read only by point lookup.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AcrossPartKey {
    /// The transaction.
    pub transaction: TransactionId,
    /// The range its part was prepared in.
    pub range: Reach,
}

impl StoreKey for AcrossPartKey {
    type Value = Sequence;

    const KIND: KeyKind = KeyKind::AcrossPart;

    fn encode(&self) -> Key {
        let mut writer = KeyWriter::with_capacity(TRANSACTION_ID_LEN.saturating_add(24));
        writer
            .put_u8(Self::KIND.tag())
            .put_fixed(&self.transaction.bytes());
        put_reach(&mut writer, self.range);
        Key::from(writer.finish())
    }

    fn decode(bytes: &[u8]) -> Result<Self> {
        let mut reader = KeyReader::new(Self::KIND, bytes);
        reader.expect_kind()?;
        let transaction = TransactionId::new(reader.take_fixed::<TRANSACTION_ID_LEN>()?);
        let range = take_reach(&mut reader)?;
        reader.finish()?;
        Ok(Self { transaction, range })
    }
}

/// A table where a committed transaction across leaders is part-way on this
/// node: readers and the table's indexes disagree about it (Q-919).
///
/// ```text
/// <0x53> <table:u32> <transaction:16>
/// ```
///
/// Index entries carry no version and are derived only by a committed
/// resolution, while a reader sees a committed transaction only once this node
/// holds every part of it (D6a). So an intent of a transaction readers already
/// see is missing from the index, and a resolution this node holds while a
/// part is still missing is in the index and hidden from readers — and a read
/// that believed the index would answer either way wrongly. The table leads so
/// a read asks about its own table with one prefix seek; a table's id is unique
/// in the store, so it needs no tenancy beside it. Written and deleted in
/// the batches that move the transaction here; the value is its committed
/// record, whose participants a later part needs when the record itself is not
/// on this node.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AcrossUnsettledKey {
    /// The table.
    pub table: TableId,
    /// The transaction.
    pub transaction: TransactionId,
}

impl AcrossUnsettledKey {
    /// The prefix every transaction unsettled in one table shares.
    #[must_use]
    pub fn prefix_of(table: TableId) -> Vec<u8> {
        let mut writer = KeyWriter::with_capacity(5);
        writer
            .put_u8(KeyKind::AcrossUnsettled.tag())
            .put_u32(table.get());
        writer.finish()
    }

    /// The prefix every key of this kind shares.
    #[must_use]
    pub fn prefix() -> Vec<u8> {
        vec![KeyKind::AcrossUnsettled.tag()]
    }
}

impl StoreKey for AcrossUnsettledKey {
    type Value = TransactionRecord;

    const KIND: KeyKind = KeyKind::AcrossUnsettled;

    fn encode(&self) -> Key {
        let mut writer = KeyWriter::with_capacity(TRANSACTION_ID_LEN.saturating_add(5));
        writer
            .put_u8(Self::KIND.tag())
            .put_u32(self.table.get())
            .put_fixed(&self.transaction.bytes());
        Key::from(writer.finish())
    }

    fn decode(bytes: &[u8]) -> Result<Self> {
        let mut reader = KeyReader::new(Self::KIND, bytes);
        reader.expect_kind()?;
        let table = TableId::new(reader.take_u32()?);
        let transaction = TransactionId::new(reader.take_fixed::<TRANSACTION_ID_LEN>()?);
        reader.finish()?;
        Ok(Self { table, transaction })
    }
}

/// One transaction across leaders' part in one range, barred by status
/// recovery before its prepare landed (ADR-0112 D14c).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AcrossBarredKey {
    /// The transaction.
    pub transaction: TransactionId,
    /// The range whose part is barred.
    pub range: Reach,
}

impl StoreKey for AcrossBarredKey {
    type Value = crate::Barred;

    const KIND: KeyKind = KeyKind::AcrossBarred;

    fn encode(&self) -> Key {
        let mut writer = KeyWriter::with_capacity(TRANSACTION_ID_LEN.saturating_add(24));
        writer
            .put_u8(Self::KIND.tag())
            .put_fixed(&self.transaction.bytes());
        put_reach(&mut writer, self.range);
        Key::from(writer.finish())
    }

    fn decode(bytes: &[u8]) -> Result<Self> {
        let mut reader = KeyReader::new(Self::KIND, bytes);
        reader.expect_kind()?;
        let transaction = TransactionId::new(reader.take_fixed::<TRANSACTION_ID_LEN>()?);
        let range = take_reach(&mut reader)?;
        reader.finish()?;
        Ok(Self { transaction, range })
    }
}

#[cfg(test)]
mod tests;
