//! Reading a topic after a position.

use std::ops::Bound;
use tessari_encoding::{StoreKey, TopicOffsetKey, decode_payload};

use tessari_kv::{Key, KeyRange, ScanDirection, ScanRequest};
use tessari_types::{DatabaseId, NamespaceId, RecordId, TableId, Value};

use crate::error::Result;
use crate::transaction::{RecordAddress, Transaction};

/// One message and the position it holds.
#[derive(Debug, Clone, PartialEq)]
pub struct Message {
    /// Its position, from 1.
    pub position: u64,
    /// Its identity.
    pub id: RecordId,
    /// What it holds.
    pub value: Value,
}

/// What a read of a topic found.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Messages {
    /// The messages, in position order.
    pub messages: Vec<Message>,
    /// Positions passed over because their retention had passed — messages the
    /// reader will never be given.
    pub lapsed: u64,
    /// The first position the topic still holds, or `None` when it holds none.
    pub first: Option<u64>,
    /// The last position the topic has given, or `None` when it has given none
    /// — removed or not.
    pub last: Option<u64>,
}

/// How many index entries one backend read takes at a time.
const PAGE: usize = 256;

impl Transaction<'_> {
    /// Up to `limit` messages of a topic after position `after`, as this
    /// transaction sees them.
    ///
    /// The positions are read from the index, which is always the newest
    /// committed state, and each message through this transaction. A position
    /// whose message this transaction does not hold at all was committed after
    /// its snapshot — and so was every position after it, since positions are
    /// given in commit order — so the read stops there rather than answering a
    /// later message before an earlier one. A message it holds but may no longer
    /// answer has passed its retention, and is counted as passed over.
    ///
    /// # Errors
    ///
    /// A backend failure, or a payload that cannot be decoded.
    pub fn topic_after(
        &self,
        namespace: NamespaceId,
        database: DatabaseId,
        table: TableId,
        after: u64,
        limit: usize,
    ) -> Result<Messages> {
        let topic = (namespace, database, table);
        let mut found = Messages {
            first: super::edge(self.store(), topic, ScanDirection::Forward)?,
            last: Some(super::head(self.store(), topic)?).filter(|head| *head > 0),
            ..Messages::default()
        };
        let Some(mut from) = after.checked_add(1) else {
            return Ok(found);
        };
        let end = KeyRange::prefix(&TopicOffsetKey::table_prefix(namespace, database, table))
            .end()
            .clone();
        'pages: while found.messages.len() < limit {
            let start = TopicOffsetKey::from_offset(namespace, database, table, from);
            let request = ScanRequest {
                keyspace: TopicOffsetKey::keyspace(),
                range: KeyRange::from_bounds(Bound::Included(Key::from_slice(&start)), end.clone()),
                direction: ScanDirection::Forward,
                limit: Some(PAGE),
            };
            let page = self.store().backend().scan(&request)?;
            if page.is_empty() {
                break;
            }
            for (key, _) in &page {
                let entry = TopicOffsetKey::decode(key.as_slice())?;
                from = entry.offset.saturating_add(1);
                let address = RecordAddress::new(namespace, database, table, entry.id.clone());
                if self.get_held(&address)?.is_none() {
                    break 'pages;
                }
                match self.get(&address)? {
                    Some(payload) => found.messages.push(Message {
                        position: entry.offset,
                        id: entry.id,
                        value: decode_payload(&payload)?,
                    }),
                    None => found.lapsed = found.lapsed.saturating_add(1),
                }
                if found.messages.len() >= limit {
                    break 'pages;
                }
            }
        }
        Ok(found)
    }
}
