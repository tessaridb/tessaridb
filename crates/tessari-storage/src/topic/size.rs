//! A topic that keeps at most a declared number of payload bytes (G055 C8).
//!
//! # Where the limit is enforced, and why there
//!
//! In the commit attempt, beside the space limit and for its reason: two
//! appends to one topic do not conflict with each other, so a total read at
//! each transaction's snapshot would let both through. Read against the
//! committed state the attempt builds on, the total is exact.
//!
//! # What removal is
//!
//! Ordinary deletes of the oldest messages, added to the appending commit's own
//! log record: a follower applies them as it applies every other mutation and
//! decides nothing itself, and a reader passed over is told so by the `lapsed`
//! note retention already gives. The committing record's own messages are never
//! removed; a commit whose own messages alone do not fit is refused, because a
//! message that vanished on commit is the loss this limit must not produce.
//!
//! # The total
//!
//! A counter beside the topic's head, written on apply as messages arrive and
//! leave. It is kept only for a topic declared with the limit, so it begins with
//! the topic's first message and is complete; a counter that is not there — a
//! store restored without it — is recounted from the messages once.

use tessari_encoding::{
    CausalStamp, LogRecord, Mutation, NODE_ID_LEN, RecordValue, StampedValue, StoreKey, StoreValue,
    TopicBytesKey, TopicOffsetKey,
};
use tessari_kv::{KeyRange, ScanDirection, ScanRequest, WriteBatch};
use tessari_types::{RecordId, Sequence};

use super::Topic;
use crate::error::{Error, Result};
use crate::store::Store;
use crate::transaction::{RecordAddress, Transaction};

/// How many position entries one page of a walk reads.
const PAGE: usize = 256;

/// The payload bytes a message holds at `view`'s snapshot, or zero when it is
/// not there.
fn message_bytes(
    view: &Transaction<'_>,
    (namespace, database, table): Topic,
    id: &RecordId,
) -> Result<u64> {
    let address = RecordAddress::new(namespace, database, table, id.clone());
    Ok(match view.read_newest_stamped(&address)? {
        Some((_, held)) => match held.value() {
            RecordValue::Present(payload) => u64::try_from(payload.len()).unwrap_or(u64::MAX),
            RecordValue::Tombstone => 0,
        },
        None => 0,
    })
}

/// Every message a topic holds, oldest first, handed to `each` until it says
/// to stop.
fn oldest_first(
    store: &Store,
    (namespace, database, table): Topic,
    mut each: impl FnMut(RecordId) -> Result<bool>,
) -> Result<()> {
    let prefix = TopicOffsetKey::table_prefix(namespace, database, table);
    let mut range = KeyRange::prefix(&prefix);
    loop {
        let batch = store.backend().scan(&ScanRequest {
            keyspace: TopicOffsetKey::keyspace(),
            range: range.clone(),
            direction: ScanDirection::Forward,
            limit: Some(PAGE),
        })?;
        let Some((last, _)) = batch.last() else {
            return Ok(());
        };
        let resume = last.clone();
        let full = batch.len() >= PAGE;
        for (key, _) in batch {
            if !each(TopicOffsetKey::decode(key.as_slice())?.id)? {
                return Ok(());
            }
        }
        if !full {
            return Ok(());
        }
        // Resumed just past the last key read: a key followed by a zero byte is
        // the smallest key greater than it.
        let mut next = resume.as_slice().to_vec();
        next.push(0);
        range = KeyRange::from_bounds(
            std::ops::Bound::Included(tessari_kv::Key::new(next)),
            range.end().clone(),
        );
    }
}

/// The payload bytes a size-retained topic holds in the committed state: its
/// counter, or a recount of its messages when the counter is not there.
///
/// # Errors
///
/// Returns an error when the backend fails or a key cannot be decoded.
pub(crate) fn held_bytes(store: &Store, view: &Transaction<'_>, topic: Topic) -> Result<u64> {
    let (namespace, database, table) = topic;
    let key = TopicBytesKey {
        namespace,
        database,
        table,
    };
    if let Some(bytes) = store
        .backend()
        .get(TopicBytesKey::keyspace(), &key.encode())?
    {
        return Ok(Sequence::decode(bytes.as_slice())?.get());
    }
    let mut total = 0_u64;
    oldest_first(store, topic, |id| {
        total = total.saturating_add(message_bytes(view, topic, &id)?);
        Ok(true)
    })?;
    Ok(total)
}

/// Bring one topic the record appends to within `limit`, adding deletes of its
/// oldest messages to `mutations`; answers whether any were added.
///
/// # Errors
///
/// [`Error::TopicMessageTooLarge`] for a message larger than the limit,
/// [`Error::TopicRetainExceeded`] for a commit whose own messages do not fit;
/// a backend error otherwise.
pub(crate) fn enforce(
    store: &Store,
    view: &Transaction<'_>,
    topic: Topic,
    (name, limit): (&str, u64),
    mutations: &mut Vec<Mutation>,
    node: [u8; NODE_ID_LEN],
) -> Result<bool> {
    let mut added = 0_u64;
    let mut removed = 0_u64;
    for mutation in mutations
        .iter()
        .filter(|m| (m.namespace, m.database, m.table) == topic)
    {
        match mutation.value.value() {
            RecordValue::Present(payload) => {
                let size = u64::try_from(payload.len()).unwrap_or(u64::MAX);
                if size > limit {
                    return Err(Error::TopicMessageTooLarge {
                        topic: name.to_owned(),
                        max: limit,
                    });
                }
                added = added.saturating_add(size);
            }
            RecordValue::Tombstone => {
                removed = removed.saturating_add(message_bytes(view, topic, &mutation.id)?);
            }
        }
    }
    if added > limit {
        return Err(Error::TopicRetainExceeded {
            topic: name.to_owned(),
            retain_bytes: limit,
        });
    }
    let held = held_bytes(store, view, topic)?.saturating_sub(removed);
    let mut over = held.saturating_add(added).saturating_sub(limit);
    if over == 0 {
        return Ok(false);
    }
    let (namespace, database, table) = topic;
    let mut evicted = Vec::new();
    // The committing record's own messages hold no position yet, and a
    // rewrite of a held one was refused above, so the walk meets only messages
    // older than the commit.
    oldest_first(store, topic, |id| {
        let size = message_bytes(view, topic, &id)?;
        let address = RecordAddress::new(namespace, database, table, id.clone());
        // The stamp is carried forward from the version being removed, advanced
        // by this node, exactly as a space's eviction carries it.
        let mut stamp = view
            .read_newest_stamped(&address)?
            .map_or_else(CausalStamp::new, |(_, stamped)| stamped.stamp().clone());
        stamp.advance(node);
        evicted.push(Mutation {
            namespace,
            database,
            table,
            id,
            shard: None,
            value: StampedValue::stamped(stamp, RecordValue::Tombstone),
        });
        over = over.saturating_sub(size);
        Ok(over > 0)
    })?;
    if over > 0 {
        return Err(Error::TopicRetainExceeded {
            topic: name.to_owned(),
            retain_bytes: limit,
        });
    }
    let any = !evicted.is_empty();
    mutations.extend(evicted);
    Ok(any)
}

/// Add the counter write a record implies for one size-retained topic to
/// `batch`: what it holds before the record, plus what arrives, less what
/// leaves.
///
/// # Errors
///
/// Returns an error when the backend fails.
pub(crate) fn count(
    store: &Store,
    view: &Transaction<'_>,
    topic: Topic,
    record: &LogRecord,
    batch: WriteBatch,
) -> Result<WriteBatch> {
    let mut total = held_bytes(store, view, topic)?;
    for mutation in record
        .mutations()
        .iter()
        .filter(|m| (m.namespace, m.database, m.table) == topic)
    {
        let before = message_bytes(view, topic, &mutation.id)?;
        total = total.saturating_sub(before);
        if let RecordValue::Present(payload) = mutation.value.value() {
            total = total.saturating_add(u64::try_from(payload.len()).unwrap_or(u64::MAX));
        }
    }
    let (namespace, database, table) = topic;
    Ok(batch.put(
        TopicBytesKey::keyspace(),
        TopicBytesKey {
            namespace,
            database,
            table,
        }
        .encode(),
        Sequence::new(total).encode(),
    ))
}

impl Transaction<'_> {
    /// The payload bytes a topic holds, for `INFO FOR TOPIC` (G055 C8).
    ///
    /// # Errors
    ///
    /// Returns an error when the backend fails.
    pub fn topic_bytes(
        &self,
        namespace: tessari_types::NamespaceId,
        database: tessari_types::DatabaseId,
        table: tessari_types::TableId,
    ) -> Result<u64> {
        held_bytes(self.store(), self, (namespace, database, table))
    }
}
