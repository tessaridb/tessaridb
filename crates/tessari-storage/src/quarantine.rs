//! The messages a Kafka consumer quarantined, kept where the language can find
//! them (Q-708, G011 S6).
//!
//! `ON FAILURE quarantine` keeps a partition moving past a message that cannot
//! be applied. Counting it in the running process was a receipt that expired
//! with the process; here each one is a system row — the payload, where it came
//! from, why it was refused and when — written in the transaction that writes
//! its batch, so the message is parked exactly when its neighbours land, and
//! `INFO FOR KAFKA CONSUMER` reads it back after any restart.
//!
//! # Bound
//!
//! [`KAFKA_QUARANTINE_HELD`] per consumer. Past it the row at the lowest
//! partition and offset goes — within a partition the oldest — so a consumer
//! fed nothing but poison keeps the newest and never grows without limit.
//!
//! # Identity
//!
//! `consumer · partition · offset`, so a redelivered batch parks its message
//! on the row it parked last time rather than beside it.

use tessari_constants::KAFKA_QUARANTINE_HELD;
use tessari_encoding::{KeyWriter, decode_payload, encode_payload};
use tessari_types::{RecordId, Value};

use crate::catalog::system::{self, KAFKA_QUARANTINE};
use crate::error::Result;
use crate::transaction::Transaction;

fn consumer_prefix(consumer: u32) -> Vec<u8> {
    let mut writer = KeyWriter::new();
    writer.put_u32(consumer);
    writer.finish()
}

fn parked_id(consumer: u32, partition: i32, offset: i64) -> RecordId {
    let mut writer = KeyWriter::new();
    writer
        .put_u32(consumer)
        .put_i64(i64::from(partition))
        .put_i64(offset);
    RecordId::Bytes(writer.finish())
}

impl Transaction<'_> {
    /// Park one message a consumer quarantined: `record` is what is kept.
    ///
    /// # Errors
    ///
    /// Returns an error when the consumer's parked messages cannot be read.
    pub fn keep_quarantined(
        &mut self,
        consumer: u32,
        partition: i32,
        offset: i64,
        record: &Value,
    ) -> Result<()> {
        let id = parked_id(consumer, partition, offset);
        let held = self.system_rows_prefixed(KAFKA_QUARANTINE, &consumer_prefix(consumer))?;
        if held.len() >= KAFKA_QUARANTINE_HELD
            && !held.iter().any(|(kept, _)| *kept == id)
            && let Some((oldest, _)) = held.into_iter().next()
        {
            self.delete(system::address(KAFKA_QUARANTINE, oldest));
        }
        self.put(
            system::address(KAFKA_QUARANTINE, id),
            encode_payload(record).into_bytes(),
        );
        Ok(())
    }

    /// Every message a consumer has parked, by partition and offset.
    ///
    /// # Errors
    ///
    /// Returns an error when the store cannot be read or a row decoded.
    pub fn quarantined(&self, consumer: u32) -> Result<Vec<Value>> {
        self.system_rows_prefixed(KAFKA_QUARANTINE, &consumer_prefix(consumer))?
            .into_iter()
            .map(|(_, payload)| Ok(decode_payload(&payload)?))
            .collect()
    }

    /// Remove every message a consumer has parked.
    ///
    /// # Errors
    ///
    /// Returns an error when the store cannot be read.
    pub fn forget_quarantined(&mut self, consumer: u32) -> Result<()> {
        for (id, _) in self.system_rows_prefixed(KAFKA_QUARANTINE, &consumer_prefix(consumer))? {
            self.delete(system::address(KAFKA_QUARANTINE, id));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use std::sync::Arc;

    use tessari_kv::{KvBackend, MemoryBackend};

    use super::*;
    use crate::Store;

    fn parked(offset: i64) -> Value {
        Value::Number(tessari_types::Number::Integer(offset))
    }

    #[test]
    fn a_consumer_keeps_its_bound_and_loses_the_oldest_first() {
        let store = Store::open(Arc::new(MemoryBackend::new()) as Arc<dyn KvBackend>).unwrap();
        let mut transaction = store.begin().unwrap();
        let held = i64::try_from(KAFKA_QUARANTINE_HELD).unwrap();
        for offset in 0..=held {
            transaction
                .keep_quarantined(7, 0, offset, &parked(offset))
                .unwrap();
        }
        // Another consumer's rows are its own.
        transaction.keep_quarantined(8, 0, 0, &parked(0)).unwrap();
        transaction.commit().unwrap();
        let transaction = store.begin().unwrap();
        let kept = transaction.quarantined(7).unwrap();
        assert_eq!(kept.len(), KAFKA_QUARANTINE_HELD);
        assert_eq!(kept.first(), Some(&parked(1)));
        assert_eq!(kept.last(), Some(&parked(held)));
        assert_eq!(transaction.quarantined(8).unwrap(), vec![parked(0)]);
    }

    #[test]
    fn a_redelivered_message_is_parked_where_it_was_and_evicts_nothing() {
        let store = Store::open(Arc::new(MemoryBackend::new()) as Arc<dyn KvBackend>).unwrap();
        let mut transaction = store.begin().unwrap();
        let held = i64::try_from(KAFKA_QUARANTINE_HELD).unwrap();
        for offset in 0..held {
            transaction
                .keep_quarantined(7, 0, offset, &parked(offset))
                .unwrap();
        }
        transaction.keep_quarantined(7, 0, 5, &parked(5)).unwrap();
        let kept = transaction.quarantined(7).unwrap();
        assert_eq!(kept.len(), KAFKA_QUARANTINE_HELD);
        assert_eq!(kept.first(), Some(&parked(0)));
        transaction.forget_quarantined(7).unwrap();
        assert!(transaction.quarantined(7).unwrap().is_empty());
    }
}
