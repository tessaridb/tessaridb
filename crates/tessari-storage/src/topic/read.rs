//! Reading a topic after a position.

use std::ops::Bound;
use tessari_encoding::{StoreKey, StoreValue, TopicMark, TopicOffsetKey, decode_payload};

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
    /// When it was appended, in milliseconds since the epoch — `None` for a
    /// message appended before its topic kept the instant (ADR-0133 D3).
    pub appended: Option<u64>,
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
    pub async fn topic_after(
        &self,
        namespace: NamespaceId,
        database: DatabaseId,
        table: TableId,
        after: u64,
        limit: usize,
    ) -> Result<Messages> {
        let topic = (namespace, database, table);
        let mut found = Messages {
            first: super::edge(self.store(), topic, ScanDirection::Forward).await?,
            last: Some(super::head(self.store(), topic).await?).filter(|head| *head > 0),
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
            let page = self.store().backend().scan(&request).await?;
            if page.is_empty() {
                break;
            }
            for (key, mark) in &page {
                let entry = TopicOffsetKey::decode(key.as_slice())?;
                from = entry.offset.saturating_add(1);
                let address = RecordAddress::new(namespace, database, table, entry.id.clone());
                if self.get_held(&address).await?.is_none() {
                    break 'pages;
                }
                match self.get(&address).await? {
                    Some(payload) => found.messages.push(Message {
                        position: entry.offset,
                        id: entry.id,
                        value: decode_payload(&payload)?,
                        appended: TopicMark::decode(mark.as_slice())?.appended,
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

impl Transaction<'_> {
    /// The first position of a topic appended at or after `instant`
    /// milliseconds, or `None` when every message it holds is older
    /// (ADR-0133 D3).
    ///
    /// Instants never decrease along a topic's positions, and the positions it
    /// holds are dense from its first, so a binary search over them answers in
    /// as many index reads as the topic has bits of length. A message appended
    /// before its topic kept the instant counts as older than every instant.
    /// The answer is read from the committed index; a position this
    /// transaction cannot see yet is one a read from it stops before.
    ///
    /// # Errors
    ///
    /// A backend failure, or an entry that cannot be decoded.
    pub async fn topic_since(
        &self,
        namespace: NamespaceId,
        database: DatabaseId,
        table: TableId,
        instant: u64,
    ) -> Result<Option<u64>> {
        let topic = (namespace, database, table);
        let Some(first) = super::edge(self.store(), topic, ScanDirection::Forward).await? else {
            return Ok(None);
        };
        let last = super::head(self.store(), topic).await?;
        let at_or_after =
            |appended: Option<u64>| appended.is_some_and(|appended| appended >= instant);
        let (mut low, mut high) = (first, last.saturating_add(1));
        // Every position below `low` is older than `instant`; the first one at
        // or after it, if any, is below `high`.
        while low < high {
            let middle = low.saturating_add(high.saturating_sub(low) / 2);
            match self.appended_at(topic, middle).await? {
                Some((_, appended)) if at_or_after(appended) => high = middle,
                Some((at, _)) => low = at.saturating_add(1),
                None => high = middle,
            }
        }
        Ok(match self.appended_at(topic, low).await? {
            Some((at, appended)) if at <= last && at_or_after(appended) => Some(at),
            _ => None,
        })
    }

    /// The first position at or after `offset` and the instant it was
    /// appended at, or `None` when the topic holds nothing from there on.
    async fn appended_at(
        &self,
        (namespace, database, table): super::Topic,
        offset: u64,
    ) -> Result<Option<(u64, Option<u64>)>> {
        let start = TopicOffsetKey::from_offset(namespace, database, table, offset);
        let end = KeyRange::prefix(&TopicOffsetKey::table_prefix(namespace, database, table))
            .end()
            .clone();
        let request = ScanRequest {
            keyspace: TopicOffsetKey::keyspace(),
            range: KeyRange::from_bounds(Bound::Included(Key::from_slice(&start)), end),
            direction: ScanDirection::Forward,
            limit: Some(1),
        };
        match self.store().backend().scan(&request).await?.first() {
            Some((key, mark)) => Ok(Some((
                TopicOffsetKey::decode(key.as_slice())?.offset,
                TopicMark::decode(mark.as_slice())?.appended,
            ))),
            None => Ok(None),
        }
    }
}

impl Transaction<'_> {
    /// The position each of `ids` holds in a topic, `None` for one that holds
    /// none here (ADR-0129 D10).
    ///
    /// Read from the index the commit maintains, so it answers for messages
    /// that have committed and been applied on this node; a message whose
    /// commit another node is still applying answers `None` rather than a
    /// guess.
    ///
    /// # Errors
    ///
    /// A backend failure.
    pub async fn topic_positions(
        &self,
        namespace: NamespaceId,
        database: DatabaseId,
        table: TableId,
        ids: &[RecordId],
    ) -> Result<Vec<Option<u64>>> {
        let mut positions = Vec::with_capacity(ids.len());
        for id in ids {
            positions
                .push(super::position_of(self.store(), (namespace, database, table), id).await?);
        }
        Ok(positions)
    }
}
