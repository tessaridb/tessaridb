//! Allocating ids and record numbers, and claiming names.

use super::system::Level;
use super::{Catalog, definition, id_key, name_of, parse_qualified, system, with_article};
use crate::error::{Error, Result};
use crate::store::Store;
use dashmap::DashMap;
use tessari_encoding::{
    LogRecord, RecordKey, RecordValue, StoreKey, StoreValue, decode_payload, encode_payload,
};
use tessari_kv::WriteBatch;
use tessari_types::{RecordId, Sequence, TableId};

/// The next id this process offers at each level, kept ahead of the stored
/// counter so two transactions open at once are never handed the same one.
///
/// In memory, because it only has to be right for the transactions this
/// process holds open: what is committed is in the counter, which
/// [`maintain`] keeps past every committed id, and a collision with another
/// process is still refused at commit by the marker both would write.
#[derive(Debug, Default)]
pub(crate) struct Allocated(DashMap<&'static str, u32>);

impl Allocated {
    /// The id to hand out at `level`, given the counter this transaction read.
    fn next(&self, level: Level, stored: u32) -> Result<u32> {
        let mut held = self.0.entry(level.counter()).or_insert(stored);
        let id = (*held).max(stored);
        *held = id.checked_add(1).ok_or(Error::IdSpaceExhausted {
            level: level.counter(),
        })?;
        Ok(id)
    }
}

/// The record that says `id` was handed out at `level`.
///
/// One per id rather than one counter per level: two definitions in unrelated
/// places write two different records, so they no longer conflict, while two
/// writers handed the same id still write the same record and one of them loses.
fn marker(level: Level, id: u32) -> RecordId {
    RecordId::from(format!("{}/{id}", level.counter()).as_str())
}

impl<'a, 'txn> Catalog<'a, 'txn> {
    /// Hand out the next id at `level`, and record that it was handed out.
    ///
    /// The transaction writes the id's own marker and never the level's counter
    /// (engine Q-959): a counter every definition wrote was a record every two
    /// concurrent definitions conflicted on, in any namespace. The counter is
    /// read here and raised by [`maintain`] when the marker commits, so a build
    /// that reads only the counter still never re-issues an id.
    pub(crate) async fn allocate(&mut self, level: Level) -> Result<u32> {
        let address = system::address(system::ALLOCATORS, RecordId::from(level.counter()));
        let stored = match self.transaction.get(&address).await? {
            Some(bytes) => definition::id_of(&decode_payload(&bytes)?, "allocator", "next")?,
            None => system::FIRST_ID,
        };
        let id = self.transaction.store().allocated().next(level, stored)?;
        self.transaction.put(
            system::address(system::ALLOCATORS, marker(level, id)),
            encode_payload(&definition::number(id)).into_bytes(),
        );
        Ok(id)
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
    pub async fn next_record_number(&mut self, table: TableId) -> Result<u64> {
        let address = system::address(system::RECORD_SEQUENCES, RecordId::Int(id_key(table.get())));
        let next = match self.transaction.get(&address).await? {
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
    pub async fn record_count(&mut self, table: TableId) -> Result<Option<u64>> {
        let address = system::address(system::RECORD_COUNTS, RecordId::Int(id_key(table.get())));
        let Some(bytes) = self.transaction.get(&address).await? else {
            return Ok(None);
        };
        Ok(Some(definition::count_of(
            &decode_payload(&bytes)?,
            "record count",
            "held",
        )?))
    }

    /// Refuse early if the name is already resolvable.
    pub(crate) async fn reserve_name(&self, qualified: &str) -> Result<()> {
        let Some(id) = self.resolve(qualified).await? else {
            return Ok(());
        };
        // Said in the caller's words: what holds the name and the name itself,
        // never the key the catalog keeps it under (Q-G042-1).
        let noun = match parse_qualified(qualified) {
            Some((Level::Table, _)) => match self.table(TableId::new(id)).await? {
                Some(table) => table.kind.noun(),
                None => Level::Table.noun(),
            },
            Some((level, _)) => level.noun(),
            None => "catalog object",
        };
        Err(Error::NameTaken {
            holder: with_article(noun),
            name: name_of(qualified).to_owned(),
        })
    }

    pub(crate) fn claim_name(&mut self, qualified: &str, id: u32) {
        let address = system::address(system::NAMES, RecordId::from(qualified));
        let value = definition::number(id);
        self.transaction
            .put(address, encode_payload(&value).into_bytes());
    }

    pub(crate) async fn resolve(&self, qualified: &str) -> Result<Option<u32>> {
        let address = system::address(system::NAMES, RecordId::from(qualified));
        let Some(bytes) = self.row(&address).await? else {
            return Ok(None);
        };
        definition::id_of(&decode_payload(&bytes)?, "name", "id").map(Some)
    }
}

/// Raise each level's counter past the ids this record's markers hand out.
///
/// Derived on apply, on the leader's commit and on every follower alike, for
/// the reason index entries are: a counter only the leader moved would let a
/// promoted follower hand out an id that already names a definition.
///
/// # Errors
///
/// Returns an error when the store cannot be read or a held counter cannot be
/// decoded.
pub(crate) async fn maintain(
    store: &Store,
    record: &LogRecord,
    mut batch: WriteBatch,
    version: Sequence,
) -> Result<WriteBatch> {
    // A handful per record at most, so a list rather than a map.
    let mut highest: Vec<(String, u32)> = Vec::new();
    for mutation in record.mutations() {
        if mutation.namespace != system::SYSTEM_NAMESPACE
            || mutation.database != system::SYSTEM_DATABASE
            || mutation.table != system::ALLOCATORS
            || !matches!(mutation.value.value(), RecordValue::Present(_))
        {
            continue;
        }
        let RecordId::Text(held) = &mutation.id else {
            continue;
        };
        let Some((counter, id)) = held.split_once('/') else {
            continue;
        };
        let Ok(id) = id.parse::<u32>() else {
            continue;
        };
        match highest.iter_mut().find(|(seen, _)| seen == counter) {
            Some((_, top)) => *top = (*top).max(id),
            None => highest.push((counter.to_owned(), id)),
        }
    }
    if highest.is_empty() {
        return Ok(batch);
    }
    let view = store.begin_local().await?;
    for (counter, id) in highest {
        let address = system::address(system::ALLOCATORS, RecordId::from(counter.as_str()));
        let stored = match view.get_held(&address).await? {
            Some(bytes) => definition::id_of(&decode_payload(&bytes)?, "allocator", "next")?,
            None => system::FIRST_ID,
        };
        // `allocate` refuses the last id a level has, so `id + 1` exists.
        let next = id.saturating_add(1);
        if stored >= next {
            continue;
        }
        let key = RecordKey::new(
            system::SYSTEM_NAMESPACE,
            system::SYSTEM_DATABASE,
            system::ALLOCATORS,
            RecordId::from(counter.as_str()),
            version,
        );
        let value = RecordValue::Present(encode_payload(&definition::number(next)).into_bytes());
        batch = batch.put(RecordKey::keyspace(), key.encode(), value.encode());
    }
    Ok(batch)
}
