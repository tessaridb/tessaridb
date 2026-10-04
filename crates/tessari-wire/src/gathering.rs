//! One shard's records, asked of its leader by a node holding part of the
//! table (G033, ADR-0083).
//!
//! # The door's half
//!
//! A node holding some of a split table's shards gathers the rest from their
//! leaders to answer a read. What crosses the peer link is a [`Gather`] — which
//! table, which shard, which window of it, and after which identity — and one
//! [`Page`] of stored records back, or a [`Ungathered`] refusal in a frame of its
//! own. Nothing is evaluated here: the asking node runs the statement, so a
//! grant's redaction and every condition are enforced where they are for a local
//! read.
//!
//! # Who may ask
//!
//! A proven peer whose declared subscription holds part of the same table — any
//! of its shards, or the database it lives in. The table is the unit of read
//! entitlement and the shard the unit of storage: an operator who placed one
//! shard of `orders` on a node has put that node in the business of answering
//! reads of `orders`. A follower confined to another tenancy asks for nothing it
//! is given.
//!
//! # One page per connection
//!
//! The link carries one follow-up per connection (see [`crate::Ask`]), so a
//! shard larger than a page is fetched a page at a time, each after the last
//! identity the previous page held. Pages are each read when asked, which is
//! one more reason a gathered answer is not one snapshot.

use tessari_constants::GATHER_PAGE_RECORDS;
use tessari_encoding::NODE_ID_LEN;
use tessari_storage::{Catalog, Reach, Store, Window};
use tessari_types::{DatabaseId, NamespaceId, RecordId, ShardId, TableId};

use crate::collection::Subscriptions;
use crate::error::{Error, Result};
use crate::frame;

mod codec;
mod counted;
mod folds;
mod ordered;
mod serving;

use codec::{
    SECTION_COUNTED, SECTION_COUNTING, SECTION_ENOUGH, SECTION_ORDERED, SECTION_PUSHED,
    SECTION_REDUCE, SECTION_REDUCED, next, put_id, put_optional, put_pushed, take_id,
    take_optional, take_pushed,
};
use folds::{
    folded, put_portable, put_reduce, put_reduced, put_visible, take_portable, take_reduce,
    take_reduced, take_visible,
};
pub(crate) use serving::serve;

/// A peer asking for one page of one shard's records.
#[derive(Debug, Clone, PartialEq)]
pub struct Gather {
    /// The table's namespace.
    pub namespace: NamespaceId,
    /// The table's database.
    pub database: DatabaseId,
    /// The table.
    pub table: TableId,
    /// The shard.
    pub shard: ShardId,
    /// The first identity wanted, inclusive; `None` for the shard's start.
    pub from: Option<RecordId>,
    /// Where the window stops and whether that identity is inside; `None` for
    /// the shard's end.
    pub to: Option<(RecordId, bool)>,
    /// The last identity already received, so the page begins past it.
    pub after: Option<RecordId>,
    /// A condition to narrow the page by, under the asker's visibility
    /// (ADR-0097). Absent from a frame an older asker sent.
    pub pushed: Option<tessari_session::Pushed>,
    /// The most records the asker still needs from this shard, in identity
    /// order (ADR-0097 D2); `None` for all of them.
    pub enough: Option<u64>,
    /// The folds to answer instead of the records (ADR-0097 D2).
    pub reduce: Option<tessari_session::Reduce>,
    /// The order whose first records are all the asker needs from this shard
    /// (ADR-0102); `None` for all of them.
    pub ordered: Option<tessari_session::Ordered>,
    /// The search index whose figures to count instead of sending records
    /// (ADR-0103); `None` for the records.
    pub counting: Option<tessari_session::Counting>,
}

/// One page of a shard's records.
#[derive(Debug, Clone, PartialEq)]
pub struct Page {
    /// Stored records in identity order.
    pub records: Vec<(RecordId, Vec<u8>)>,
    /// Whether the answer stopped at a bound rather than at the window's end.
    pub more: bool,
    /// Where the next page begins when it is not the last record sent: a page
    /// narrowed by a pushed condition may keep none of the records it read.
    pub resume: Option<RecordId>,
    /// What the page's records folded into, when the asker sent folds.
    pub reduced: Option<tessari_session::Reduced>,
    /// What the page's records hold for a search index, when the asker sent
    /// a count (ADR-0103).
    pub counted: Option<tessari_storage::SearchCounts>,
}

/// Why a shard's leader would not answer.
///
/// Three conditions with three repairs, so three values: a `REPLICATES` that
/// holds part of the table, a different node, and a catalog that has caught up.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Ungathered {
    /// The asking peer's subscription holds nothing of this table.
    NotEntitled,
    /// This node does not hold the shard.
    NotHeld,
    /// This node's catalog has no such table or shard.
    NoSuchTable,
    /// This node's map of the table no longer has — or does not yet have — the
    /// shard asked for: one of the two maps moved, and the asker reads its own
    /// again (ADR-0095 D4).
    MapMoved,
}

impl Ungathered {
    /// Numbered explicitly and never renumbered.
    pub(crate) const fn byte(self) -> u8 {
        match self {
            Self::NotEntitled => 1,
            Self::NotHeld => 2,
            Self::NoSuchTable => 3,
            Self::MapMoved => 4,
        }
    }

    /// Recover one; an unknown byte is refused rather than read as one of them.
    pub(crate) fn from_byte(byte: u8) -> Result<Self> {
        match byte {
            1 => Ok(Self::NotEntitled),
            2 => Ok(Self::NotHeld),
            3 => Ok(Self::NoSuchTable),
            4 => Ok(Self::MapMoved),
            _ => Err(Error::Malformed),
        }
    }
}

impl core::fmt::Display for Ungathered {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(match self {
            Self::NotEntitled => "the asking node's subscription holds no part of this table",
            Self::NotHeld => "the node asked does not hold this shard",
            Self::NoSuchTable => "the node asked has no such table or shard",
            Self::MapMoved => "the node asked holds a different map of this table's shards",
        })
    }
}

impl Gather {
    /// The body of a `Gather` frame.
    #[must_use]
    pub fn encode(&self) -> Vec<u8> {
        let mut body = Vec::new();
        frame::put_u32(&mut body, self.namespace.get());
        frame::put_u32(&mut body, self.database.get());
        frame::put_u32(&mut body, self.table.get());
        frame::put_u32(&mut body, self.shard.get());
        put_optional(&mut body, self.from.as_ref());
        match &self.to {
            Some((id, inclusive)) => {
                body.push(if *inclusive { 2 } else { 1 });
                put_id(&mut body, id);
            }
            None => body.push(0),
        }
        put_optional(&mut body, self.after.as_ref());
        if let Some(pushed) = &self.pushed {
            body.push(SECTION_PUSHED);
            put_pushed(&mut body, pushed);
        }
        if let Some(enough) = self.enough {
            body.push(SECTION_ENOUGH);
            frame::put_u64(&mut body, enough);
        }
        if let Some(reduce) = &self.reduce {
            body.push(SECTION_REDUCE);
            put_reduce(&mut body, reduce);
        }
        if let Some(ordered) = &self.ordered {
            body.push(SECTION_ORDERED);
            ordered::put_ordered(&mut body, ordered);
        }
        if let Some(counting) = &self.counting {
            body.push(SECTION_COUNTING);
            counted::put_counting(&mut body, counting);
        }
        body
    }

    /// Read one back; anything left over is refused.
    ///
    /// # Errors
    ///
    /// [`Error::Malformed`] for a body that is not one `Gather`.
    pub fn decode(body: &[u8]) -> Result<Self> {
        let (namespace, at) = frame::take_u32(body, 0)?;
        let (database, at) = frame::take_u32(body, at)?;
        let (table, at) = frame::take_u32(body, at)?;
        let (shard, at) = frame::take_u32(body, at)?;
        let (from, at) = take_optional(body, at)?;
        let (to, at) = match body.get(at) {
            Some(0) => (None, next(at)?),
            Some(flag @ (1 | 2)) => {
                let (id, at) = take_id(body, next(at)?)?;
                (Some((id, *flag == 2)), at)
            }
            _ => return Err(Error::Malformed),
        };
        let (after, at) = take_optional(body, at)?;
        // Trailing sections, each at most once and in this order; a frame an
        // older asker sent has none.
        let (pushed, at) = match body.get(at) {
            Some(&SECTION_PUSHED) => {
                let (pushed, at) = take_pushed(body, next(at)?)?;
                (Some(pushed), at)
            }
            _ => (None, at),
        };
        let (enough, at) = match body.get(at) {
            Some(&SECTION_ENOUGH) => {
                let (enough, at) = frame::take_u64(body, next(at)?)?;
                (Some(enough), at)
            }
            _ => (None, at),
        };
        let (reduce, at) = match body.get(at) {
            Some(&SECTION_REDUCE) => {
                let (reduce, at) = take_reduce(body, next(at)?)?;
                (Some(reduce), at)
            }
            _ => (None, at),
        };
        let (ordered, at) = match body.get(at) {
            Some(&SECTION_ORDERED) => {
                let (ordered, at) = ordered::take_ordered(body, next(at)?)?;
                (Some(ordered), at)
            }
            _ => (None, at),
        };
        let (counting, at) = match body.get(at) {
            Some(&SECTION_COUNTING) => {
                let (counting, at) = counted::take_counting(body, next(at)?)?;
                (Some(counting), at)
            }
            _ => (None, at),
        };
        if at != body.len() {
            return Err(Error::Malformed);
        }
        Ok(Self {
            pushed,
            enough,
            reduce,
            ordered,
            counting,
            namespace: NamespaceId::new(namespace),
            database: DatabaseId::new(database),
            table: TableId::new(table),
            shard: ShardId::new(shard),
            from,
            to,
            after,
        })
    }
}

impl Page {
    /// The body of a `Gathered` frame.
    #[must_use]
    pub fn encode(&self) -> Vec<u8> {
        let mut body = vec![u8::from(self.more)];
        frame::put_u32(
            &mut body,
            u32::try_from(self.records.len()).unwrap_or(u32::MAX),
        );
        for (id, record) in &self.records {
            put_id(&mut body, id);
            frame::put_bytes(&mut body, record);
        }
        if self.resume.is_some() {
            put_optional(&mut body, self.resume.as_ref());
        }
        if let Some(reduced) = &self.reduced {
            body.push(SECTION_REDUCED);
            put_reduced(&mut body, reduced);
        }
        if let Some(counted) = &self.counted {
            body.push(SECTION_COUNTED);
            counted::put_counted(&mut body, counted);
        }
        body
    }

    /// Read one back.
    ///
    /// # Errors
    ///
    /// [`Error::Malformed`] for a body that is not one page.
    pub fn decode(body: &[u8]) -> Result<Self> {
        let more = match body.first() {
            Some(0) => false,
            Some(1) => true,
            _ => return Err(Error::Malformed),
        };
        let (count, mut at) = frame::take_u32(body, 1)?;
        let mut records = Vec::new();
        for _ in 0..count {
            let (id, next_at) = take_id(body, at)?;
            let (record, next_at) = frame::take_bytes(body, next_at)?;
            records.push((id, record));
            at = next_at;
        }
        // Written only when there is one, so a section that says *none* is not
        // a page this side sent.
        let (resume, at) = match body.get(at) {
            Some(1) => match take_optional(body, at)? {
                (Some(resume), at) => (Some(resume), at),
                (None, _) => return Err(Error::Malformed),
            },
            _ => (None, at),
        };
        let (reduced, at) = match body.get(at) {
            Some(&SECTION_REDUCED) => {
                let (reduced, at) = take_reduced(body, next(at)?)?;
                (Some(reduced), at)
            }
            _ => (None, at),
        };
        let (counted, at) = match body.get(at) {
            Some(&SECTION_COUNTED) => {
                let (counted, at) = counted::take_counted(body, next(at)?)?;
                (Some(counted), at)
            }
            _ => (None, at),
        };
        if at != body.len() {
            return Err(Error::Malformed);
        }
        Ok(Self {
            records,
            more,
            resume,
            reduced,
            counted,
        })
    }
}

#[cfg(test)]
mod tests;

/// G033 S2.1 — the door, over a real handshake, out of a real store.
#[cfg(test)]
mod door;
