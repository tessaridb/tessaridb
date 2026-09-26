//! A topic's order: every message holds one position, dense from 1 (G037).
//!
//! # Where a position is decided
//!
//! In the commit, inside each attempt, against the committed state that attempt
//! builds on — the place a limited space is enforced (G036) and for its
//! reason. An attempt is applied only if the committed tail has not moved since
//! it read that state, so "the last position plus one" read here is exact and
//! two concurrent appenders can never be given the same position or leave one
//! out. A follower derives the same positions from the same log, because it
//! applies one writer's commits in the order they were made (ADR-0084).
//!
//! # What cannot be written to a topic
//!
//! A message is never rewritten and never deleted by a caller: a correction is a
//! new message. Both are refused here, at the commit, so that no statement path
//! — there are many that write a record — can bypass it. The one deletion a
//! topic accepts is of a message whose retention has passed, which is what the
//! node's housekeeping issues.

mod rate;
mod read;

pub(crate) use rate::PublicRates;

use std::collections::{BTreeMap, BTreeSet};

use tessari_encoding::{
    ExpiryMark, LogRecord, RecordValue, StoreKey, StoreValue, TopicEntryKey, TopicHeadKey,
    TopicOffsetKey,
};
use tessari_kv::{KeyRange, ScanDirection, ScanRequest, WriteBatch};
use tessari_types::{DatabaseId, NamespaceId, RecordId, Sequence, TableId};

use crate::catalog::{Catalog, TableKind, TopicDeclaration};
use crate::error::{Error, Result};
use crate::store::Store;
use crate::transaction::{RecordAddress, Transaction};

pub use read::{Message, Messages};

type Topic = (NamespaceId, DatabaseId, TableId);

/// The name and declaration of each topic a record writes.
fn topics_in(
    view: &mut Transaction<'_>,
    record: &LogRecord,
) -> Result<BTreeMap<Topic, (String, TopicDeclaration)>> {
    let mut found = BTreeMap::new();
    let mut seen = BTreeSet::new();
    for mutation in record.mutations() {
        let topic = (mutation.namespace, mutation.database, mutation.table);
        if !seen.insert(topic) {
            continue;
        }
        let Some(definition) = Catalog::new(view).table(mutation.table)? else {
            continue;
        };
        if definition.namespace != mutation.namespace || definition.database != mutation.database {
            continue;
        }
        if let TableKind::Topic(declared) = definition.kind {
            found.insert(topic, (definition.name, declared));
        }
    }
    Ok(found)
}

/// The position a message holds, if it holds one.
pub(crate) fn position_of(
    store: &Store,
    (namespace, database, table): Topic,
    id: &RecordId,
) -> Result<Option<u64>> {
    let request = ScanRequest {
        keyspace: TopicEntryKey::keyspace(),
        range: KeyRange::prefix(&TopicEntryKey::message_prefix(
            namespace, database, table, id,
        )),
        direction: ScanDirection::Forward,
        limit: Some(1),
    };
    match store.backend().scan(&request)?.first() {
        Some((key, _)) => Ok(Some(TopicEntryKey::decode(key.as_slice())?.offset)),
        None => Ok(None),
    }
}

/// The last position a topic has given, or 0 when it has given none.
pub(crate) fn head(store: &Store, (namespace, database, table): Topic) -> Result<u64> {
    let key = TopicHeadKey {
        namespace,
        database,
        table,
    };
    match store
        .backend()
        .get(TopicHeadKey::keyspace(), &key.encode())?
    {
        Some(bytes) => Ok(Sequence::decode(bytes.as_slice())?.get()),
        None => Ok(0),
    }
}

/// The first or last position a topic holds, or `None` when it holds none.
pub(crate) fn edge(
    store: &Store,
    (namespace, database, table): Topic,
    direction: ScanDirection,
) -> Result<Option<u64>> {
    let request = ScanRequest {
        keyspace: TopicOffsetKey::keyspace(),
        range: KeyRange::prefix(&TopicOffsetKey::table_prefix(namespace, database, table)),
        direction,
        limit: Some(1),
    };
    match store.backend().scan(&request)?.first() {
        Some((key, _)) => Ok(Some(TopicOffsetKey::decode(key.as_slice())?.offset)),
        None => Ok(None),
    }
}

/// Admit a record's writes to the topics it touches.
///
/// Refuses a rewrite, a deletion whose retention has not passed at `now`, and
/// a message larger than its topic takes; answers the record carrying each new
/// message's expiry when its topic keeps messages for a while, or `None` when
/// nothing needed adding. Called once per commit attempt, beside the space
/// limit, for the same reason: it is the one place every write path passes.
///
/// # Errors
///
/// [`Error::TopicIsAppendOnly`] or [`Error::TopicMessageTooLarge`] naming the
/// topic; a backend or catalog error otherwise.
pub(crate) fn admit(store: &Store, record: &LogRecord, now: u64) -> Result<Option<LogRecord>> {
    let mut view = store.begin()?;
    let topics = topics_in(&mut view, record)?;
    if topics.is_empty() {
        return Ok(None);
    }
    let mut mutations = record.mutations().to_vec();
    let mut stamped = false;
    for mutation in &mut mutations {
        let topic = (mutation.namespace, mutation.database, mutation.table);
        let Some((name, declared)) = topics.get(&topic) else {
            continue;
        };
        let address = RecordAddress::new(topic.0, topic.1, topic.2, mutation.id.clone());
        let held = view
            .read_newest_stamped(&address)?
            .filter(|(_, held)| !held.value().is_tombstone());
        let payload = match mutation.value.value() {
            RecordValue::Present(payload) => payload.len(),
            RecordValue::Tombstone => {
                if held.is_some_and(|(_, held)| !held.is_expired_at(now)) {
                    return Err(Error::TopicIsAppendOnly {
                        topic: name.clone(),
                    });
                }
                continue;
            }
        };
        if held.is_some() {
            return Err(Error::TopicIsAppendOnly {
                topic: name.clone(),
            });
        }
        if let Some(max) = declared.max_bytes
            && u64::try_from(payload).unwrap_or(u64::MAX) > max
        {
            return Err(Error::TopicMessageTooLarge {
                topic: name.clone(),
                max,
            });
        }
        if let Some(retain) = declared.retain
            && mutation.value.expires().is_none()
        {
            let at = millis(retain)
                .and_then(|span| now.checked_add(span))
                .ok_or(Error::TopicPositionsExhausted)?;
            mutation.value = mutation.value.clone().expiring(at);
            stamped = true;
        }
    }
    if !stamped {
        return Ok(None);
    }
    let mut carried = LogRecord::at(record.epoch(), mutations);
    if let Some(order) = record.order() {
        carried.set_order(order);
    }
    Ok(Some(carried))
}

/// A duration in whole milliseconds, or `None` when it does not fit.
fn millis(span: tessari_types::Duration) -> Option<u64> {
    let seconds = u64::try_from(span.seconds()).ok()?;
    seconds
        .checked_mul(1000)?
        .checked_add(u64::from(span.nanos() / 1_000_000))
}

/// Add the position writes a log record implies to `batch`.
///
/// A new message takes the next position in the order the record carries its
/// mutations; a removed message gives up both of its entries. A rewrite — which
/// a commit refuses and a follower may still meet in a log written before this
/// rule — keeps the position the message already held.
pub(crate) fn maintain(
    store: &Store,
    record: &LogRecord,
    mut batch: WriteBatch,
) -> Result<WriteBatch> {
    let mut view = store.begin()?;
    let topics = topics_in(&mut view, record)?;
    if topics.is_empty() {
        return Ok(batch);
    }
    let mut next: BTreeMap<Topic, u64> = BTreeMap::new();
    for mutation in record.mutations() {
        let topic = (mutation.namespace, mutation.database, mutation.table);
        if !topics.contains_key(&topic) {
            continue;
        }
        let held = position_of(store, topic, &mutation.id)?;
        match (held, mutation.value.value()) {
            (None, RecordValue::Present(_)) => {
                let offset = match next.get(&topic) {
                    Some(offset) => *offset,
                    None => head(store, topic)?
                        .checked_add(1)
                        .ok_or(Error::TopicPositionsExhausted)?,
                };
                batch = put_entries(batch, topic, &mutation.id, offset);
                let following = offset
                    .checked_add(1)
                    .ok_or(Error::TopicPositionsExhausted)?;
                next.insert(topic, following);
            }
            (Some(offset), RecordValue::Tombstone) => {
                let (namespace, database, table) = topic;
                batch = batch
                    .delete(
                        TopicOffsetKey::keyspace(),
                        TopicOffsetKey {
                            namespace,
                            database,
                            table,
                            offset,
                            id: mutation.id.clone(),
                        }
                        .encode(),
                    )
                    .delete(
                        TopicEntryKey::keyspace(),
                        TopicEntryKey {
                            namespace,
                            database,
                            table,
                            id: mutation.id.clone(),
                            offset,
                        }
                        .encode(),
                    );
            }
            _ => {}
        }
    }
    for ((namespace, database, table), following) in next {
        let key = TopicHeadKey {
            namespace,
            database,
            table,
        };
        batch = batch.put(
            TopicHeadKey::keyspace(),
            key.encode(),
            Sequence::new(following.saturating_sub(1)).encode(),
        );
    }
    Ok(batch)
}

fn put_entries(
    batch: WriteBatch,
    (namespace, database, table): Topic,
    id: &RecordId,
    offset: u64,
) -> WriteBatch {
    batch
        .put(
            TopicOffsetKey::keyspace(),
            TopicOffsetKey {
                namespace,
                database,
                table,
                offset,
                id: id.clone(),
            }
            .encode(),
            ExpiryMark.encode(),
        )
        .put(
            TopicEntryKey::keyspace(),
            TopicEntryKey {
                namespace,
                database,
                table,
                id: id.clone(),
                offset,
            }
            .encode(),
            ExpiryMark.encode(),
        )
}
