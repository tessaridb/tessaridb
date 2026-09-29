//! Allocating ids and record numbers, and claiming names.

use super::system::Level;
use super::{Catalog, definition, id_key, system};
use crate::error::{Error, Result};
use tessari_encoding::{decode_payload, encode_payload};
use tessari_types::{RecordId, TableId};

impl<'a, 'txn> Catalog<'a, 'txn> {
    /// Hand out the next id at `level`, and record that it was handed out.
    ///
    /// The counter is written in this transaction, so two concurrent creations
    /// write the same record and one of them loses — the same mechanism that
    /// keeps names unique, and no second lock.
    pub(crate) fn allocate(&mut self, level: Level) -> Result<u32> {
        let address = system::address(system::ALLOCATORS, RecordId::from(level.counter()));
        let next = match self.transaction.get(&address)? {
            Some(bytes) => definition::id_of(&decode_payload(&bytes)?, "allocator", "next")?,
            None => system::FIRST_ID,
        };
        let following = next.checked_add(1).ok_or(Error::IdSpaceExhausted {
            level: level.counter(),
        })?;
        self.transaction.put(
            address,
            encode_payload(&definition::number(following)).into_bytes(),
        );
        Ok(next)
    }

    /// Hand out the next identity for a record in `table`, and record that it
    /// was handed out.
    ///
    /// Written in the caller's transaction for the reason [`Self::allocate`]
    /// gives: two writers that each read the same counter also both write it,
    /// and that shared key is what makes one of them lose. So no number reaches
    /// two records, and no second lock is needed to say so.
    ///
    /// The counter is a catalog record like every other, so it rides the log,
    /// takes the snapshot and reaches a replica — which is the whole point. A
    /// counter a replica derived for itself, or one restored from a backup taken
    /// before the writes it counts, would re-issue an identity that already
    /// names a record, and the next write under it would replace that record
    /// rather than add one, with nothing anywhere in an error state.
    ///
    /// The number answered always fits an `i64`, so the caller can build a
    /// [`RecordId::Int`] from it without a second refusal to invent.
    ///
    /// # Errors
    ///
    /// Returns [`Error::IdSpaceExhausted`] when the table has spent every
    /// identity the key grammar can express, and a substrate or decoding
    /// failure otherwise.
    pub fn next_record_number(&mut self, table: TableId) -> Result<u64> {
        let address = system::address(system::RECORD_SEQUENCES, RecordId::Int(id_key(table.get())));
        let next = match self.transaction.get(&address)? {
            Some(bytes) => {
                definition::count_of(&decode_payload(&bytes)?, "record sequence", "next")?
            }
            None => system::FIRST_RECORD_NUMBER,
        };
        let following = next.checked_add(1).ok_or(Error::IdSpaceExhausted {
            level: definition::RECORD_LEVEL,
        })?;
        // Stored before it is answered, so a count this store could hold but
        // could never spend is refused while the caller still has no identity to
        // do anything with.
        let held = definition::count(following)?;
        self.transaction
            .put(address, encode_payload(&held).into_bytes());
        Ok(next)
    }

    /// How many records a table holds, when the store has a count for it.
    ///
    /// `None` means no estimate rather than an empty table: the counter is
    /// written by [`crate::cardinality`] when a record arrives or leaves, so a
    /// table nothing has written since the store was created has no record
    /// here. The two are worth telling apart because a planner told "no
    /// estimate" must fall back to the behaviour it had before counts existed,
    /// while a planner told "zero" would conclude that every index beats a scan
    /// of nothing.
    ///
    /// It is an **estimate for choosing an access path** and never an answer.
    /// Nothing that decides which records a statement returns may read it.
    ///
    /// # Errors
    ///
    /// Returns an error when the store cannot be read or the stored count
    /// cannot be decoded.
    pub fn record_count(&mut self, table: TableId) -> Result<Option<u64>> {
        let address = system::address(system::RECORD_COUNTS, RecordId::Int(id_key(table.get())));
        let Some(bytes) = self.transaction.get(&address)? else {
            return Ok(None);
        };
        Ok(Some(definition::count_of(
            &decode_payload(&bytes)?,
            "record count",
            "held",
        )?))
    }

    /// Refuse early if the name is already resolvable.
    pub(crate) fn reserve_name(&self, qualified: &str) -> Result<()> {
        if self.resolve(qualified)?.is_some() {
            return Err(Error::NameTaken {
                qualified: qualified.to_owned(),
            });
        }
        Ok(())
    }

    pub(crate) fn claim_name(&mut self, qualified: &str, id: u32) {
        let address = system::address(system::NAMES, RecordId::from(qualified));
        let value = definition::number(id);
        self.transaction
            .put(address, encode_payload(&value).into_bytes());
    }

    pub(crate) fn resolve(&self, qualified: &str) -> Result<Option<u32>> {
        let address = system::address(system::NAMES, RecordId::from(qualified));
        let Some(bytes) = self.row(&address)? else {
            return Ok(None);
        };
        definition::id_of(&decode_payload(&bytes)?, "name", "id").map(Some)
    }
}
