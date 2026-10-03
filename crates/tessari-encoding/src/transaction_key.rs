//! The key of one transaction across leaders' record (ADR-0112).

use tessari_kv::Key;

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

#[cfg(test)]
mod tests;
