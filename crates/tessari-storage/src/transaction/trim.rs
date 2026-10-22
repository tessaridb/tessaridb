//! `TRIM`: removing a topic's oldest messages, and only those (W8c).
//!
//! A message is never deleted by a caller (G037), with one exception: the
//! front of a topic, all of it at once. The deletions are buffered like any
//! other, beside the bound they reach, and the commit admits a topic message's
//! deletion only when this transaction trimmed its topic past it
//! (`topic::admit`). Every message below the bound is deleted in the same call
//! that grants the permission, and the set below a bound only shrinks before
//! the commit — retention removes from the same front, and a new message takes
//! a higher position — so what a trim leaves is dense from its first message.

use tessari_encoding::{StoreKey, TopicOffsetKey};
use tessari_kv::{Key, KeyRange, ScanDirection, ScanRequest};
use tessari_types::{DatabaseId, NamespaceId, TableId};

use super::{RecordAddress, Transaction};
use crate::error::Result;

/// How many entries one page of the walk reads.
const PAGE: usize = 1_000;

/// How far a trim reaches.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TopicTrim {
    /// Every message but the newest this many.
    Keep(u64),
    /// Every message below this position.
    Before(u64),
}

impl Transaction<'_> {
    /// Delete `topic`'s messages as `trim` reaches, and answer how many.
    ///
    /// # Errors
    ///
    /// Returns an error when the topic's index cannot be read.
    pub async fn trim_topic(
        &mut self,
        topic: (NamespaceId, DatabaseId, TableId),
        trim: TopicTrim,
    ) -> Result<u64> {
        let (namespace, database, table) = topic;
        let below = match trim {
            TopicTrim::Before(below) => below,
            TopicTrim::Keep(keep) => {
                match crate::topic::edge(self.store, topic, ScanDirection::Reverse).await? {
                    Some(last) => last.saturating_add(1).saturating_sub(keep),
                    None => return Ok(0),
                }
            }
        };
        let reach = KeyRange::between(
            Key::from(TopicOffsetKey::table_prefix(namespace, database, table)),
            Key::from(TopicOffsetKey::from_offset(
                namespace, database, table, below,
            )),
        );
        let mut removed = 0_u64;
        let mut after: Option<Key> = None;
        loop {
            let request = ScanRequest {
                keyspace: TopicOffsetKey::keyspace(),
                range: after
                    .as_ref()
                    .map_or_else(|| reach.clone(), |after| reach.resuming_after(after)),
                direction: ScanDirection::Forward,
                limit: Some(PAGE),
            };
            let page = self.store.backend().scan(&request).await?;
            let last = page.len() < PAGE;
            for (key, _) in page {
                let entry = TopicOffsetKey::decode(key.as_slice())?;
                after = Some(key);
                self.delete(RecordAddress::new(namespace, database, table, entry.id));
                removed = removed.saturating_add(1);
            }
            if last {
                break;
            }
        }
        let held = self.trims.entry(topic).or_insert(below);
        *held = (*held).max(below);
        Ok(removed)
    }
}
