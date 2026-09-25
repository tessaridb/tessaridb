//! A space that holds at most a declared number of keys (G036).
//!
//! # Where the limit is enforced, and why there
//!
//! In the commit, inside each attempt, against the committed state that attempt
//! builds on — the place the schema is validated and conflicts are checked, and
//! for their reason. Two transactions adding two different keys to a space one
//! short of its limit do not conflict with each other, so a check made at each
//! transaction's own snapshot would let both through and leave the space over
//! its limit with nothing in an error state. An attempt is applied only if the
//! committed tail has not moved since it read that state, so its count is exact.
//!
//! # What eviction is
//!
//! Ordinary deletes, added to the attempt's own log record: a follower applies
//! them as it applies every other mutation and decides nothing itself. The keys
//! removed are the least recently **modified** ones — the first entries of the
//! space's modified-order index — and never one the committing transaction
//! wrote. A read writes nothing, so it does not make a key recent. A commit that
//! alone adds more keys than the limit is refused rather than trimmed.

use std::collections::{BTreeMap, BTreeSet};

use tessari_encoding::{
    CausalStamp, ExpiryMark, LogRecord, ModifiedKey, Mutation, NODE_ID_LEN, RecordValue,
    StampedValue, StoreKey, StoreValue,
};
use tessari_kv::{KeyRange, ScanDirection, ScanRequest, WriteBatch};
use tessari_types::{DatabaseId, NamespaceId, RecordId, Sequence, TableId};

use crate::catalog::{Catalog, Eviction, SpaceDeclaration, SpaceLimit, TableKind};
use crate::error::{Error, Result};
use crate::store::Store;
use crate::transaction::{RecordAddress, Transaction};

type Space = (NamespaceId, DatabaseId, TableId);

/// The limit of each table a record writes that is a limited space.
fn limits_in(
    view: &mut Transaction<'_>,
    record: &LogRecord,
) -> Result<BTreeMap<Space, (String, SpaceLimit)>> {
    let mut found = BTreeMap::new();
    let mut seen = BTreeSet::new();
    for mutation in record.mutations() {
        let space = (mutation.namespace, mutation.database, mutation.table);
        if !seen.insert(space) {
            continue;
        }
        let Some(definition) = Catalog::new(view).table(mutation.table)? else {
            continue;
        };
        if definition.namespace != mutation.namespace || definition.database != mutation.database {
            continue;
        }
        if let TableKind::Space(SpaceDeclaration { limit: Some(limit) }) = definition.kind {
            found.insert(space, (definition.name, limit));
        }
    }
    Ok(found)
}

/// Keep each limited space the record writes within its limit.
///
/// Answers the record to write: the same one when nothing is over, or one
/// carrying the evictions as well. Called once per commit attempt.
///
/// # Errors
///
/// [`Error::SpaceFull`] for a space declared `EVICT NONE` that the record would
/// take past its limit; a backend or catalog error otherwise.
pub(crate) fn enforce(
    store: &Store,
    record: &LogRecord,
    node: [u8; NODE_ID_LEN],
) -> Result<Option<LogRecord>> {
    let mut view = store.begin()?;
    let limits = limits_in(&mut view, record)?;
    if limits.is_empty() {
        return Ok(None);
    }
    let mut evictions = Vec::new();
    for (space, (name, limit)) in limits {
        let (namespace, database, table) = space;
        let held = Catalog::new(&mut view).record_count(table)?.unwrap_or(0);
        let mut added = 0_u64;
        let mut removed = 0_u64;
        let mut written = BTreeSet::new();
        for mutation in record
            .mutations()
            .iter()
            .filter(|m| (m.namespace, m.database, m.table) == space)
        {
            written.insert(mutation.id.clone());
            let address = RecordAddress::new(namespace, database, table, mutation.id.clone());
            match (view.get_held(&address)?.is_some(), mutation.value.value()) {
                (false, RecordValue::Present(_)) => added = added.saturating_add(1),
                (true, RecordValue::Tombstone) => removed = removed.saturating_add(1),
                _ => {}
            }
        }
        let after = held.saturating_add(added).saturating_sub(removed);
        let Some(excess) = after.checked_sub(limit.max).filter(|excess| *excess > 0) else {
            continue;
        };
        if limit.eviction == Eviction::Refuse {
            return Err(Error::SpaceFull {
                space: name,
                max: limit.max,
            });
        }
        let chosen = oldest(store, &view, space, &written, excess, node)?;
        // A commit that alone writes more new keys than the space holds cannot
        // be brought within the limit without dropping some of its own writes,
        // and a write that vanishes on commit is the silent loss this rule is
        // not allowed to produce — so it is refused, as `EVICT NONE` would be.
        if u64::try_from(chosen.len()).unwrap_or(u64::MAX) < excess {
            return Err(Error::SpaceFull {
                space: name,
                max: limit.max,
            });
        }
        evictions.extend(chosen);
    }
    if evictions.is_empty() {
        return Ok(None);
    }
    let mut mutations = record.mutations().to_vec();
    mutations.extend(evictions);
    let mut carried = LogRecord::at(record.epoch(), mutations);
    if let Some(order) = record.order() {
        carried.set_order(order);
    }
    Ok(Some(carried))
}

/// Deletes for the `count` least recently modified keys of a space that the
/// committing record does not itself write.
fn oldest(
    store: &Store,
    view: &Transaction<'_>,
    (namespace, database, table): Space,
    written: &BTreeSet<RecordId>,
    count: u64,
    node: [u8; NODE_ID_LEN],
) -> Result<Vec<Mutation>> {
    let wanted = usize::try_from(count).unwrap_or(usize::MAX);
    let request = ScanRequest {
        keyspace: ModifiedKey::keyspace(),
        range: KeyRange::prefix(&ModifiedKey::table_prefix(namespace, database, table)),
        direction: ScanDirection::Forward,
        limit: Some(wanted.saturating_add(written.len())),
    };
    let mut found = Vec::with_capacity(wanted);
    for (key, _) in store.backend().scan(&request)? {
        if found.len() >= wanted {
            break;
        }
        let entry = ModifiedKey::decode(key.as_slice())?;
        if written.contains(&entry.id) {
            continue;
        }
        let address = RecordAddress::new(namespace, database, table, entry.id.clone());
        // The stamp is carried forward from the version being removed, advanced
        // by this node, exactly as every other write in the record is.
        let mut stamp = view
            .read_newest_stamped(&address)?
            .map_or_else(CausalStamp::new, |(_, stamped)| stamped.stamp().clone());
        stamp.advance(node);
        found.push(Mutation {
            namespace,
            database,
            table,
            id: entry.id,
            shard: None,
            value: StampedValue::stamped(stamp, RecordValue::Tombstone),
        });
    }
    Ok(found)
}

/// Add the modified-order writes a log record implies to `batch`, the record
/// being written at `version`.
pub(crate) fn maintain(
    store: &Store,
    record: &LogRecord,
    mut batch: WriteBatch,
    version: Sequence,
) -> Result<WriteBatch> {
    let mut view = store.begin()?;
    let limits = limits_in(&mut view, record)?;
    if limits.is_empty() {
        return Ok(batch);
    }
    for mutation in record.mutations() {
        let space = (mutation.namespace, mutation.database, mutation.table);
        if !limits.contains_key(&space) {
            continue;
        }
        let address = RecordAddress::new(space.0, space.1, space.2, mutation.id.clone());
        let entry = |at: Sequence| ModifiedKey {
            namespace: space.0,
            database: space.1,
            table: space.2,
            version: at,
            id: mutation.id.clone(),
        };
        if let Some((previous, stamped)) = view.read_newest_stamped(&address)?
            && !stamped.value().is_tombstone()
        {
            batch = batch.delete(ModifiedKey::keyspace(), entry(previous).encode());
        }
        if !mutation.value.value().is_tombstone() {
            batch = batch.put(
                ModifiedKey::keyspace(),
                entry(version).encode(),
                ExpiryMark.encode(),
            );
        }
    }
    Ok(batch)
}
