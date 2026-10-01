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

mod counted;
mod folds;
mod ordered;

use folds::{
    folded, put_portable, put_reduce, put_reduced, put_visible, take_portable, take_reduce,
    take_reduced, take_visible,
};

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

/// Answer `asked` for the peer the handshake proved to be `asker`, from `store`.
///
/// # Errors
///
/// [`Error::NotGathered`] with the reason when the peer may not have the shard
/// or this node cannot give it, and [`Error::Refused`] when the store cannot be
/// read.
pub(crate) fn serve(
    store: &Store,
    granted: &dyn Subscriptions,
    asker: [u8; NODE_ID_LEN],
    asked: &Gather,
    (budget, fold_records): (usize, usize),
) -> Result<Page> {
    let refused = |why: tessari_storage::Error| Error::Refused {
        message: why.to_string(),
    };
    let mut transaction = store.begin().map_err(refused)?;
    let definition = Catalog::new(&mut transaction)
        .table(asked.table)
        .map_err(refused)?
        .filter(|found| found.namespace == asked.namespace && found.database == asked.database);
    let Some(map) = definition.and_then(|found| found.shards) else {
        return Err(Error::NotGathered(Ungathered::NoSuchTable));
    };
    // The table is here and split, so a shard missing from its live spans is a
    // map that moved on one side or the other — retired here, or minted by a
    // change this node has not applied yet (ADR-0095 D4).
    let Some(span) = map.spans().find(|span| span.id == asked.shard) else {
        return Err(Error::NotGathered(Ungathered::MapMoved));
    };
    let shard_of = |shard| Reach::Shard(asked.namespace, asked.database, asked.table, shard);
    let entitled = granted.granted(asker)?.is_some_and(|over| {
        over.contains(Reach::Database(asked.namespace, asked.database))
            || map.spans().any(|each| over.contains(shard_of(each.id)))
    });
    if !entitled {
        return Err(Error::NotGathered(Ungathered::NotEntitled));
    }
    if store
        .served()
        .is_some_and(|over| !over.contains(shard_of(asked.shard)))
    {
        return Err(Error::NotGathered(Ungathered::NotHeld));
    }
    // Clamped to the shard's own span, so a window reaching past it is answered
    // with this shard's records and never a neighbour's this node may not hold.
    let from = match (span.from, asked.from.as_ref()) {
        (Some(start), Some(wanted)) => Some(if wanted > start { wanted } else { start }),
        (start, wanted) => wanted.or(start),
    };
    let to = match (span.to, asked.to.as_ref()) {
        (Some(end), Some((wanted, inclusive))) => Some(if wanted < end {
            (wanted, *inclusive)
        } else {
            (end, false)
        }),
        (Some(end), None) => Some((end, false)),
        (None, Some((wanted, inclusive))) => Some((wanted, *inclusive)),
        (None, None) => None,
    };
    if let Some(reduce) = &asked.reduce {
        let found = transaction
            .records_between(
                asked.namespace,
                asked.database,
                asked.table,
                Window { from, to },
                asked.after.as_ref(),
                fold_records,
            )
            .map_err(refused)?;
        transaction.rollback();
        let more = found.len() == fold_records;
        return folded(store, reduce, (found, more), budget);
    }
    if let Some(counting) = &asked.counting {
        let page = counted::counted_page(
            &mut transaction,
            asked,
            counting,
            Window { from, to },
            fold_records,
        );
        transaction.rollback();
        return page;
    }
    if let Some(ordered) = &asked.ordered {
        let page = ordered::ranked_page(
            store,
            &mut transaction,
            asked,
            ordered,
            Window { from, to },
            budget,
        );
        transaction.rollback();
        return page;
    }
    let found = transaction
        .records_between(
            asked.namespace,
            asked.database,
            asked.table,
            Window { from, to },
            asked.after.as_ref(),
            GATHER_PAGE_RECORDS,
        )
        .map_err(refused)?;
    transaction.rollback();
    let more = found.len() == GATHER_PAGE_RECORDS;
    // ADR-0097: narrowed after the page is read and before it is budgeted, so
    // what the condition drops costs nothing to send. The page then resumes
    // after the last record READ, which may be one nobody kept.
    let read_to = found.last().map(|(id, _)| id.clone());
    let narrowed = asked.pushed.is_some();
    let found = match &asked.pushed {
        Some(pushed) => {
            tessari_session::keeping(store, pushed, found).map_err(|why| Error::Refused {
                message: why.to_string(),
            })?
        }
        None => found,
    };
    // Enough is a promise about the asker's need, not a budget: the records past
    // it are not sent, and nothing follows them.
    let enough = asked
        .enough
        .map(|enough| usize::try_from(enough).unwrap_or(usize::MAX));
    let (found, more) = match enough {
        Some(enough) if found.len() >= enough => {
            let mut found = found;
            found.truncate(enough);
            (found, false)
        }
        _ => (found, more),
    };
    let mut more = more;
    let mut cut = false;
    let mut records = Vec::with_capacity(found.len());
    let mut bytes = 0_usize;
    for (id, record) in found {
        // At least one record per page, whatever its size, or a record larger
        // than the budget could never be fetched at all.
        if !records.is_empty() && bytes.saturating_add(record.len()) > budget {
            more = true;
            cut = true;
            break;
        }
        bytes = bytes.saturating_add(record.len());
        records.push((id, record));
    }
    // Cut by the budget, the next page begins after the last record sent, as it
    // always has; narrowed and not cut, after the last record read.
    let resume = if narrowed && more && !cut {
        read_to
    } else {
        None
    };
    Ok(Page {
        records,
        more,
        resume,
        reduced: None,
        counted: None,
    })
}

/// The section of a `Gather` frame carrying a pushed condition.
const SECTION_PUSHED: u8 = 1;
/// The section of a `Gather` frame carrying how many records are enough.
const SECTION_ENOUGH: u8 = 2;
/// The section of a `Gather` frame carrying the folds to answer instead.
const SECTION_REDUCE: u8 = 3;
/// The section of a `Gather` frame carrying the order to rank by (ADR-0102).
const SECTION_ORDERED: u8 = 4;
/// The section of a `Gather` frame carrying the search index to count
/// (ADR-0103).
const SECTION_COUNTING: u8 = 5;
/// The section of a `Gathered` frame carrying what the records folded into —
/// past the optional resume, whose own first byte is `1`.
const SECTION_REDUCED: u8 = 3;
/// The section of a `Gathered` frame carrying what the page counted
/// (ADR-0103).
const SECTION_COUNTED: u8 = 4;

/// A pushed condition: the visible fields, then the condition as an expression.
fn put_pushed(into: &mut Vec<u8>, pushed: &tessari_session::Pushed) {
    put_visible(into, &pushed.visible);
    put_portable(into, &pushed.condition, &pushed.parameters);
}

fn take_pushed(from: &[u8], at: usize) -> Result<(tessari_session::Pushed, usize)> {
    let (visible, at) = take_visible(from, at)?;
    let ((condition, parameters), at) = take_portable(from, at)?;
    Ok((
        tessari_session::Pushed {
            visible,
            condition,
            parameters,
        },
        at,
    ))
}

fn next(at: usize) -> Result<usize> {
    at.checked_add(1).ok_or(Error::Malformed)
}

fn put_optional(into: &mut Vec<u8>, id: Option<&RecordId>) {
    match id {
        Some(id) => {
            into.push(1);
            put_id(into, id);
        }
        None => into.push(0),
    }
}

fn take_optional(from: &[u8], at: usize) -> Result<(Option<RecordId>, usize)> {
    match from.get(at) {
        Some(0) => Ok((None, next(at)?)),
        Some(1) => {
            let (id, at) = take_id(from, next(at)?)?;
            Ok((Some(id), at))
        }
        _ => Err(Error::Malformed),
    }
}

/// A record identity on the peer link: its kind, then its value.
///
/// Exhaustive over [`RecordId`], so an identity kind added later is a compile
/// error here rather than a value this link cannot carry.
fn put_id(into: &mut Vec<u8>, id: &RecordId) {
    match id {
        RecordId::Int(value) => {
            into.push(1);
            into.extend_from_slice(&value.to_be_bytes());
        }
        RecordId::Text(value) => {
            into.push(2);
            frame::put_text(into, value);
        }
        RecordId::Uuid(value) => {
            into.push(3);
            into.extend_from_slice(value);
        }
        RecordId::Bytes(value) => {
            into.push(4);
            frame::put_bytes(into, value);
        }
    }
}

fn take_id(from: &[u8], at: usize) -> Result<(RecordId, usize)> {
    let body = next(at)?;
    match from.get(at) {
        Some(1) => {
            let (value, end) = frame::take_u64(from, body)?;
            Ok((RecordId::Int(i64::from_be_bytes(value.to_be_bytes())), end))
        }
        Some(2) => {
            let (value, end) = frame::take_text(from, body)?;
            Ok((RecordId::Text(value), end))
        }
        Some(3) => {
            let end = body.checked_add(16).ok_or(Error::Malformed)?;
            let mut value = [0_u8; 16];
            value.copy_from_slice(from.get(body..end).ok_or(Error::Malformed)?);
            Ok((RecordId::Uuid(value), end))
        }
        Some(4) => {
            let (value, end) = frame::take_bytes(from, body)?;
            Ok((RecordId::Bytes(value), end))
        }
        _ => Err(Error::Malformed),
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    #[test]
    fn a_gather_and_a_page_round_trip_and_a_cut_body_is_refused() {
        let asked = Gather {
            namespace: NamespaceId::new(1),
            database: DatabaseId::new(2),
            table: TableId::new(3),
            shard: ShardId::new(4),
            from: Some(RecordId::Text("g".to_owned())),
            to: Some((RecordId::Int(-7), true)),
            after: Some(RecordId::Uuid([9; 16])),
            pushed: None,
            enough: None,
            reduce: None,
            ordered: None,
            counting: None,
        };
        let body = asked.encode();
        assert_eq!(Gather::decode(&body).unwrap(), asked);
        assert!(matches!(
            Gather::decode(body.get(..body.len() - 1).unwrap()),
            Err(Error::Malformed)
        ));
        let open = Gather {
            from: None,
            to: None,
            after: None,
            ..asked
        };
        assert_eq!(Gather::decode(&open.encode()).unwrap(), open);
        let narrowed = Gather {
            pushed: Some(tessari_session::Pushed {
                visible: Some(["total".to_owned()].into()),
                condition: "(total > $p0)".to_owned(),
                parameters: [("p0".to_owned(), tessari_types::Value::from(3_i64))].into(),
            }),
            ..open.clone()
        };
        assert_eq!(Gather::decode(&narrowed.encode()).unwrap(), narrowed);
        let bounded = Gather {
            enough: Some(3),
            ..narrowed.clone()
        };
        assert_eq!(Gather::decode(&bounded.encode()).unwrap(), bounded);
        let only_bounded = Gather {
            pushed: None,
            ..bounded
        };
        assert_eq!(
            Gather::decode(&only_bounded.encode()).unwrap(),
            only_bounded
        );
        let folding = Gather {
            enough: Some(3),
            reduce: Some(folds(Some("(total > $p0)"))),
            ..narrowed.clone()
        };
        assert_eq!(Gather::decode(&folding.encode()).unwrap(), folding);
        let only_folding = Gather {
            reduce: Some(folds(None)),
            ..open.clone()
        };
        assert_eq!(
            Gather::decode(&only_folding.encode()).unwrap(),
            only_folding
        );
        let ranked = Gather {
            ordered: Some(by_n(true, 2)),
            ..narrowed.clone()
        };
        assert_eq!(Gather::decode(&ranked.encode()).unwrap(), ranked);
        // A direction that is neither is a frame read wrongly, not a third one.
        let mut sideways = ranked.encode();
        let at = sideways.len() - 9;
        assert_eq!(sideways.get(at), Some(&1));
        *sideways.get_mut(at).unwrap() = 7;
        assert!(matches!(Gather::decode(&sideways), Err(Error::Malformed)));

        let page = Page {
            records: vec![
                (RecordId::Bytes(vec![0, 1]), vec![5, 6, 7]),
                (RecordId::Text("h".to_owned()), Vec::new()),
            ],
            more: true,
            resume: None,
            reduced: None,
            counted: None,
        };
        let body = page.encode();
        assert_eq!(Page::decode(&body).unwrap(), page);
        let mut long = body.clone();
        long.push(0);
        assert!(matches!(Page::decode(&long), Err(Error::Malformed)));
        let resuming = Page {
            resume: Some(RecordId::Text("z".to_owned())),
            ..page
        };
        assert_eq!(Page::decode(&resuming.encode()).unwrap(), resuming);
        let folded = Page {
            records: Vec::new(),
            reduced: Some(tessari_session::Reduced::Partials(vec![
                tessari_session::Partial {
                    key: vec![tessari_types::Value::from("x"), tessari_types::Value::None],
                    first: RecordId::Text("a".to_owned()),
                    states: vec![tessari_types::Value::from(2_i64)],
                },
            ])),
            ..resuming.clone()
        };
        assert_eq!(Page::decode(&folded.encode()).unwrap(), folded);
        let declined = Page {
            resume: None,
            reduced: Some(tessari_session::Reduced::Declined),
            ..folded
        };
        assert_eq!(Page::decode(&declined.encode()).unwrap(), declined);
        // A fold this build does not merge exactly is not read as another one.
        let mut unknown = only_folding.encode();
        let at = unknown
            .windows(5)
            .position(|held| held == b"count")
            .unwrap();
        unknown.splice(at..at + 5, *b"blurt");
        assert!(matches!(Gather::decode(&unknown), Err(Error::Malformed)));
    }

    /// Ranked by `n`, keeping `most`.
    pub(super) fn by_n(descending: bool, most: u64) -> tessari_session::Ordered {
        tessari_session::Ordered {
            visible: None,
            keys: vec![tessari_session::OrderKey {
                key: "n".to_owned(),
                parameters: tessari_session::Parameters::new(),
                descending,
            }],
            most,
        }
    }

    /// `count(*)` and `sum(total)`, under an optional condition over `$p0`.
    fn folds(condition: Option<&str>) -> tessari_session::Reduce {
        let text = |text: &str| (text.to_owned(), tessari_session::Parameters::new());
        tessari_session::Reduce {
            visible: Some(["total".to_owned()].into()),
            condition: condition.map(|condition| {
                (
                    condition.to_owned(),
                    [("p0".to_owned(), tessari_types::Value::from(3_i64))].into(),
                )
            }),
            keys: vec![text("note")],
            folds: vec![
                tessari_session::Folded::named("count", None).unwrap(),
                tessari_session::Folded::named("sum", Some(text("total"))).unwrap(),
            ],
        }
    }

    #[test]
    fn a_count_and_its_answer_round_trip_and_a_term_the_body_lacks_is_refused() {
        let asked = Gather {
            namespace: NamespaceId::new(1),
            database: DatabaseId::new(2),
            table: TableId::new(3),
            shard: ShardId::new(4),
            from: None,
            to: None,
            after: Some(RecordId::from("b")),
            pushed: None,
            enough: None,
            reduce: None,
            ordered: None,
            counting: Some(tessari_session::Counting {
                index: tessari_types::IndexId::new(7),
                terms: ["fox".to_owned(), "dog".to_owned()].to_vec(),
            }),
        };
        let body = asked.encode();
        assert_eq!(Gather::decode(&body).unwrap(), asked);
        assert!(matches!(
            Gather::decode(body.get(..body.len() - 1).unwrap()),
            Err(Error::Malformed)
        ));
        let page = Page {
            records: Vec::new(),
            more: true,
            resume: Some(RecordId::from("c")),
            reduced: None,
            counted: Some(tessari_storage::SearchCounts {
                documents: 3,
                tokens: 8,
                holding: vec![2, 0],
            }),
        };
        let body = page.encode();
        assert_eq!(Page::decode(&body).unwrap(), page);
        // A count claiming a third term the body does not carry.
        let mut claimed = body.clone();
        let at = claimed.len() - 2 * 8 - 4;
        *claimed.get_mut(at + 3).unwrap() = 3;
        assert!(matches!(Page::decode(&claimed), Err(Error::Malformed)));
    }

    #[test]
    fn every_reason_keeps_its_byte_and_an_unknown_one_is_refused() {
        for reason in [
            Ungathered::NotEntitled,
            Ungathered::NotHeld,
            Ungathered::NoSuchTable,
            Ungathered::MapMoved,
        ] {
            assert_eq!(Ungathered::from_byte(reason.byte()).unwrap(), reason);
        }
        assert!(matches!(Ungathered::from_byte(0), Err(Error::Malformed)));
        assert!(matches!(Ungathered::from_byte(5), Err(Error::Malformed)));
    }
}

/// G033 S2.1 — the door, over a real handshake, out of a real store.
#[cfg(test)]
mod door {
    #![allow(clippy::unwrap_used, clippy::panic)]

    use std::net::SocketAddr;
    use std::sync::Arc;
    use std::thread::JoinHandle;

    use tessari_encoding::NODE_ID_LEN;
    use tessari_storage::{Catalog, Reach};
    use tessari_types::{DatabaseId, NamespaceId, RecordId, ShardId, TableId};
    use tessaridb::Db;

    use super::{Gather, Page, Ungathered};
    use crate::collection::{Serving, Subscriptions};
    use crate::error::{Error, Result};
    use crate::grant::Deciding;
    use crate::link::tests::{Authority, THERE, hello, settled};
    use crate::link::{Answered, Ask, Peers, call};
    use crate::peer::Purpose;

    const LEADER: [u8; NODE_ID_LEN] = [71_u8; NODE_ID_LEN];

    /// A catalog answer a test chooses.
    #[derive(Debug)]
    struct Granting(Option<Reach>);

    impl Subscriptions for Granting {
        fn granted(&self, _follower: [u8; NODE_ID_LEN]) -> Result<Option<Reach>> {
            Ok(self.0)
        }
    }

    fn leader() -> (Arc<Db>, TableId) {
        let db = Db::in_memory().unwrap();
        db.session()
            .run(
                "DEFINE NAMESPACE prod; USE NAMESPACE prod; DEFINE DATABASE shop; \
                 USE DATABASE shop; \
                 DEFINE TABLE ledger (n int) IDENTITY uuid SPLIT AT 'g'; \
                 CREATE ledger:'a' = { n: 1 }; CREATE ledger:'b' = { n: 2 }; \
                 CREATE ledger:'c' = { n: 3 }; CREATE ledger:'h' = { n: 4 };",
            )
            .unwrap();
        let table = {
            let mut transaction = db.store().begin().unwrap();
            Catalog::new(&mut transaction)
                .table_id(NamespaceId::new(1), DatabaseId::new(1), "ledger")
                .unwrap()
                .unwrap()
        };
        (Arc::new(db), table)
    }

    fn shard(table: TableId, id: u32) -> Reach {
        Reach::Shard(
            NamespaceId::new(1),
            DatabaseId::new(1),
            table,
            ShardId::new(id),
        )
    }

    /// A door for `LEADER` answering `rounds` connections out of `db`.
    fn door(
        authority: &Authority,
        db: &Arc<Db>,
        granted: Option<Reach>,
        budget: usize,
        rounds: usize,
    ) -> (SocketAddr, JoinHandle<()>) {
        folding_door(
            authority,
            db,
            granted,
            (budget, tessari_constants::GATHER_FOLD_RECORDS),
            rounds,
        )
    }

    /// The same, folding at most `fold` records into a page of groups.
    fn folding_door(
        authority: &Authority,
        db: &Arc<Db>,
        granted: Option<Reach>,
        (budget, fold): (usize, usize),
        rounds: usize,
    ) -> (SocketAddr, JoinHandle<()>) {
        let peers = Peers::bind(
            "127.0.0.1:0",
            authority.issue(LEADER, Purpose::Peer),
            &authority.der(),
        )
        .unwrap();
        let address = peers.address().unwrap();
        let mine = hello(LEADER);
        let db = Arc::clone(db);
        let handle = std::thread::spawn(move || {
            let granting = Granting(granted);
            for _ in 0..rounds {
                drop(peers.greet(
                    || Ok(mine),
                    &LEADER,
                    &Deciding::holding(settled()),
                    &Serving::within(db.store(), &granting, budget).folding_by(fold),
                ));
            }
        });
        (address, handle)
    }

    fn ask(authority: &Authority, address: SocketAddr, gather: &Gather) -> Result<Page> {
        match call(
            address,
            authority.issue(THERE, Purpose::Peer),
            &authority.der(),
            LEADER,
            &hello(THERE),
            Ask::Gather(gather),
        )?
        .1
        {
            Answered::Gathered(page) => Ok(page),
            other => panic!("answered {other:?}"),
        }
    }

    fn asking(table: TableId, shard: u32) -> Gather {
        Gather {
            namespace: NamespaceId::new(1),
            database: DatabaseId::new(1),
            table,
            shard: ShardId::new(shard),
            from: None,
            to: None,
            after: None,
            pushed: None,
            enough: None,
            reduce: None,
            ordered: None,
            counting: None,
        }
    }

    #[test]
    fn a_peer_holding_part_of_the_table_is_given_another_shard_page_by_page() {
        let authority = Authority::new();
        let (db, table) = leader();
        // A budget of one byte: every page carries exactly one record.
        let (address, handle) = door(&authority, &db, Some(shard(table, 2)), 1, 3);
        let mut gather = asking(table, 1);
        let mut received: Vec<RecordId> = Vec::new();
        let mut pages = 0;
        loop {
            let page = ask(&authority, address, &gather).unwrap();
            pages += 1;
            received.extend(page.records.iter().map(|(id, _)| id.clone()));
            if !page.more {
                break;
            }
            gather.after = received.last().cloned();
        }
        // Asserted before the door is joined: a page that ignored its budget
        // ends the conversation early, and joining first would wait forever on
        // connections that never come instead of failing here.
        assert_eq!(
            received,
            ["a", "b", "c"].map(RecordId::from).to_vec(),
            "shard 1 whole, and not shard 2's 'h'"
        );
        assert_eq!(
            pages, 3,
            "one record a page, the last saying no more follow"
        );
        handle.join().unwrap();
    }

    #[test]
    fn an_ordered_ask_is_answered_with_the_shards_first_records_page_by_page() {
        // ADR-0102: shard 1 holds a (1), b (2) and c (3); its first two by `n`
        // descending are c and b, sent in identity order. A budget of one byte
        // puts each on its own page, so the second page is ranked again and
        // begins past the first.
        let authority = Authority::new();
        let (db, table) = leader();
        let (address, handle) = door(&authority, &db, Some(shard(table, 2)), 1, 2);
        let mut gather = Gather {
            ordered: Some(super::tests::by_n(true, 2)),
            ..asking(table, 1)
        };
        let first = ask(&authority, address, &gather).unwrap();
        gather.after = first.records.last().map(|(id, _)| id.clone());
        let second = ask(&authority, address, &gather).unwrap();
        handle.join().unwrap();
        let ids = |page: &Page| -> Vec<RecordId> {
            page.records.iter().map(|(id, _)| id.clone()).collect()
        };
        assert_eq!(ids(&first), [RecordId::from("b")]);
        assert!(first.more);
        assert_eq!(ids(&second), [RecordId::from("c")]);
        assert!(!second.more);
    }

    #[test]
    fn a_window_is_answered_inside_the_shard_and_never_past_it() {
        let authority = Authority::new();
        let (db, table) = leader();
        let (address, handle) = door(&authority, &db, Some(shard(table, 2)), 1 << 20, 1);
        let gather = Gather {
            from: Some(RecordId::from("b")),
            to: Some((RecordId::from("z"), true)),
            ..asking(table, 1)
        };
        let page = ask(&authority, address, &gather).unwrap();
        handle.join().unwrap();
        let ids: Vec<RecordId> = page.records.into_iter().map(|(id, _)| id).collect();
        assert_eq!(ids, ["b", "c"].map(RecordId::from).to_vec());
        assert!(!page.more);
    }

    #[test]
    fn a_peer_holding_nothing_of_the_table_is_refused_by_name() {
        let authority = Authority::new();
        let (db, table) = leader();
        let (address, handle) = door(
            &authority,
            &db,
            Some(Reach::Namespace(NamespaceId::new(9))),
            1 << 20,
            1,
        );
        let refused = ask(&authority, address, &asking(table, 1));
        handle.join().unwrap();
        assert!(
            matches!(refused, Err(Error::NotGathered(Ungathered::NotEntitled))),
            "{refused:?}"
        );
    }

    #[test]
    fn a_holder_lacking_the_shard_or_the_table_says_which() {
        let authority = Authority::new();
        let (db, table) = leader();
        db.store().record_served(shard(table, 2)).unwrap();
        let (address, handle) = door(&authority, &db, Some(Reach::Store), 1 << 20, 2);
        let not_held = ask(&authority, address, &asking(table, 1));
        let no_table = ask(&authority, address, &asking(TableId::new(999), 1));
        handle.join().unwrap();
        assert!(
            matches!(not_held, Err(Error::NotGathered(Ungathered::NotHeld))),
            "{not_held:?}"
        );
        assert!(
            matches!(no_table, Err(Error::NotGathered(Ungathered::NoSuchTable))),
            "{no_table:?}"
        );
    }

    #[test]
    fn a_shard_the_answerers_map_has_moved_past_is_refused_as_moved() {
        // ADR-0095 D4: an asker holding an older map names a shard the answerer
        // has retired. *No such table* would send it looking for a table; the
        // repair is to read the map again, so the refusal says the map moved.
        let authority = Authority::new();
        let (db, table) = leader();
        db.session()
            .run("USE NAMESPACE prod; USE DATABASE shop; ALTER TABLE ledger SPLIT AT 'c';")
            .unwrap();
        let (address, handle) = door(&authority, &db, Some(Reach::Store), 1 << 20, 1);
        let retired = ask(&authority, address, &asking(table, 1));
        handle.join().unwrap();
        assert!(
            matches!(retired, Err(Error::NotGathered(Ungathered::MapMoved))),
            "{retired:?}"
        );
    }

    fn narrowed(condition: &str, bound: i64, visible: Option<&str>) -> tessari_session::Pushed {
        tessari_session::Pushed {
            visible: visible.map(|field| [field.to_owned()].into()),
            condition: condition.to_owned(),
            parameters: [("p0".to_owned(), tessari_types::Value::from(bound))].into(),
        }
    }

    fn ids(page: &Page) -> Vec<RecordId> {
        page.records.iter().map(|(id, _)| id.clone()).collect()
    }

    #[test]
    fn a_pushed_condition_narrows_the_page_before_it_travels() {
        let authority = Authority::new();
        let (db, table) = leader();
        let (address, handle) = door(&authority, &db, Some(Reach::Store), 1 << 20, 1);
        let page = ask(
            &authority,
            address,
            &Gather {
                pushed: Some(narrowed("(n > $p0)", 1, None)),
                ..asking(table, 1)
            },
        )
        .unwrap();
        handle.join().unwrap();
        assert_eq!(ids(&page), ["b", "c"].map(RecordId::from).to_vec());
        assert!(!page.more);
    }

    #[test]
    fn a_field_the_asker_cannot_see_is_not_searchable_through_the_leader() {
        // ADR-0097 D1: the asker's grant shows it `other` and not `n`, so `n` is
        // absent to the condition exactly as it is to the asker's own read —
        // and the leader keeps nothing rather than revealing which records
        // carry an `n` above 1.
        let authority = Authority::new();
        let (db, table) = leader();
        let (address, handle) = door(&authority, &db, Some(Reach::Store), 1 << 20, 1);
        let page = ask(
            &authority,
            address,
            &Gather {
                pushed: Some(narrowed("(n > $p0)", 1, Some("other"))),
                ..asking(table, 1)
            },
        )
        .unwrap();
        handle.join().unwrap();
        assert_eq!(ids(&page), Vec::<RecordId>::new());
    }

    #[test]
    fn narrowed_pages_still_end_where_the_budget_cuts_them() {
        let authority = Authority::new();
        let (db, table) = leader();
        let (address, handle) = door(&authority, &db, Some(Reach::Store), 1, 2);
        let mut gather = Gather {
            pushed: Some(narrowed("(n >= $p0)", 2, None)),
            ..asking(table, 1)
        };
        let first = ask(&authority, address, &gather).unwrap();
        assert_eq!(
            (ids(&first), first.more, first.resume.clone()),
            (vec![RecordId::from("b")], true, None)
        );
        gather.after = Some(RecordId::from("b"));
        let second = ask(&authority, address, &gather).unwrap();
        handle.join().unwrap();
        assert_eq!(
            (ids(&second), second.more),
            (vec![RecordId::from("c")], false)
        );
    }

    #[test]
    fn a_page_that_keeps_nothing_it_read_says_where_it_got_to() {
        let authority = Authority::new();
        let db = Db::in_memory().unwrap();
        let mut script = String::from(
            "DEFINE NAMESPACE prod; USE NAMESPACE prod; DEFINE DATABASE shop; USE DATABASE shop; \
             DEFINE TABLE ledger (n int) IDENTITY uuid SPLIT AT 'g';",
        );
        for n in 0..1100 {
            script.push_str(&format!(" CREATE ledger:'a{n:04}' = {{ n: {n} }};"));
        }
        db.session().run(&script).unwrap();
        let table = {
            let mut transaction = db.store().begin().unwrap();
            Catalog::new(&mut transaction)
                .table_id(NamespaceId::new(1), DatabaseId::new(1), "ledger")
                .unwrap()
                .unwrap()
        };
        let db = Arc::new(db);
        let (address, handle) = door(&authority, &db, Some(Reach::Store), 1 << 20, 2);
        let mut gather = Gather {
            pushed: Some(narrowed("(n >= $p0)", 1095, None)),
            ..asking(table, 1)
        };
        let first = ask(&authority, address, &gather).unwrap();
        assert!(first.records.is_empty() && first.more, "{:?}", ids(&first));
        assert_eq!(
            first.resume,
            Some(RecordId::from("a1023")),
            "resumes after the last record read"
        );
        gather.after = first.resume;
        let second = ask(&authority, address, &gather).unwrap();
        handle.join().unwrap();
        assert_eq!(ids(&second).len(), 5);
        assert!(!second.more);
    }

    /// ADR-0097 D2: asked for folds, a leader sends groups and no record, a
    /// page of records at a time, each page resuming where its read stopped.
    #[test]
    fn a_shard_is_folded_page_by_page_and_no_record_travels() {
        let authority = Authority::new();
        let (db, table) = leader();
        let (address, handle) =
            folding_door(&authority, &db, Some(shard(table, 2)), (1 << 20, 2), 2);
        let count_and_sum = || tessari_session::Reduce {
            visible: None,
            condition: None,
            keys: Vec::new(),
            folds: vec![
                tessari_session::Folded::named("count", None).unwrap(),
                tessari_session::Folded::named(
                    "sum",
                    Some(("n".to_owned(), tessari_session::Parameters::new())),
                )
                .unwrap(),
            ],
        };
        let first = ask(
            &authority,
            address,
            &Gather {
                reduce: Some(count_and_sum()),
                ..asking(table, 1)
            },
        )
        .unwrap();
        let id = |text: &str| RecordId::Text(text.to_owned());
        let states = |page: &Page| match &page.reduced {
            Some(tessari_session::Reduced::Partials(partials)) => partials
                .iter()
                .map(|partial| (partial.first.clone(), partial.states.first().cloned()))
                .collect::<Vec<_>>(),
            other => panic!("{other:?}"),
        };
        assert!(first.records.is_empty() && first.more, "{first:?}");
        assert_eq!(first.resume, Some(id("b")));
        assert_eq!(
            states(&first),
            vec![(id("a"), Some(tessari_types::Value::from(2_i64)))]
        );
        let second = ask(
            &authority,
            address,
            &Gather {
                reduce: Some(count_and_sum()),
                after: first.resume.clone(),
                ..asking(table, 1)
            },
        )
        .unwrap();
        handle.join().unwrap();
        assert!(second.records.is_empty() && !second.more, "{second:?}");
        assert_eq!(second.resume, None);
        assert_eq!(
            states(&second),
            vec![(id("c"), Some(tessari_types::Value::from(1_i64)))]
        );
    }

    /// ADR-0103: a shard's search figures, counted on its leader a page of
    /// records at a time and summed by the asker, are what the leader's own
    /// index holds for those records.
    #[test]
    fn a_shards_search_figures_are_counted_page_by_page_and_no_record_travels() {
        let db = Db::in_memory().unwrap();
        db.session()
            .run(
                "DEFINE NAMESPACE prod; USE NAMESPACE prod; DEFINE DATABASE shop; \
                 USE DATABASE shop; \
                 DEFINE ANALYZER english FILTERS lowercase, ascii, stemmer; \
                 DEFINE TABLE docs (body string ANALYZER english) IDENTITY uuid SPLIT AT 'g'; \
                 DEFINE INDEX by_body ON docs FIELDS body SEARCH; \
                 CREATE docs:'a' = { body: 'fox jumps' }; \
                 CREATE docs:'b' = { body: 'a fox and a fox' }; \
                 CREATE docs:'c' = { body: 'dogs' }; CREATE docs:'h' = { body: 'fox' };",
            )
            .unwrap();
        let (table, index) = {
            let mut transaction = db.store().begin().unwrap();
            let table = Catalog::new(&mut transaction)
                .table_id(NamespaceId::new(1), DatabaseId::new(1), "docs")
                .unwrap()
                .unwrap();
            let index = Catalog::new(&mut transaction)
                .indexes_on(table)
                .unwrap()
                .into_iter()
                .find(|index| index.search)
                .unwrap();
            (table, index.id)
        };
        let db = Arc::new(db);
        let authority = Authority::new();
        let (address, handle) =
            folding_door(&authority, &db, Some(shard(table, 2)), (1 << 20, 2), 2);
        let mut gather = Gather {
            counting: Some(tessari_session::Counting {
                index,
                terms: ["fox".to_owned(), "dog".to_owned()].to_vec(),
            }),
            ..asking(table, 1)
        };
        let first = ask(&authority, address, &gather).unwrap();
        gather.after = first.resume.clone();
        let second = ask(&authority, address, &gather).unwrap();
        handle.join().unwrap();
        assert!(first.records.is_empty() && first.more, "{first:?}");
        assert_eq!(first.resume, Some(RecordId::from("b")));
        assert!(second.records.is_empty() && !second.more, "{second:?}");
        let counted = |page: &Page| page.counted.clone().unwrap();
        // a and b: two documents, 2 + 5 tokens, both say fox; then c: one
        // document of one token, `dog` once stemmed. Not h, which is shard 2.
        assert_eq!(
            counted(&first),
            tessari_storage::SearchCounts {
                documents: 2,
                tokens: 7,
                holding: vec![2, 0],
            }
        );
        assert_eq!(
            counted(&second),
            tessari_storage::SearchCounts {
                documents: 1,
                tokens: 1,
                holding: vec![0, 1],
            }
        );
    }

    /// A page of groups past the byte budget declines rather than being cut.
    #[test]
    fn a_page_of_groups_past_the_budget_declines() {
        let authority = Authority::new();
        let (db, table) = leader();
        let (address, handle) = door(&authority, &db, Some(shard(table, 2)), 1, 1);
        let page = ask(
            &authority,
            address,
            &Gather {
                reduce: Some(tessari_session::Reduce {
                    visible: None,
                    condition: None,
                    keys: Vec::new(),
                    folds: vec![tessari_session::Folded::named("count", None).unwrap()],
                }),
                ..asking(table, 1)
            },
        )
        .unwrap();
        handle.join().unwrap();
        assert_eq!(page.reduced, Some(tessari_session::Reduced::Declined));
        assert!(page.records.is_empty() && !page.more, "{page:?}");
    }

    /// G050 C4: what a gather moves, in bytes of `Gathered` page bodies, for
    /// one shard of 1 100 records read whole, bounded to 3, and narrowed to 5.
    #[test]
    fn a_bounded_or_narrowed_gather_moves_fewer_bytes() {
        let authority = Authority::new();
        let db = Db::in_memory().unwrap();
        let mut script = String::from(
            "DEFINE NAMESPACE prod; USE NAMESPACE prod; DEFINE DATABASE shop; USE DATABASE shop; \
             DEFINE TABLE ledger (n int) IDENTITY uuid SPLIT AT 'g';",
        );
        for n in 0..1100 {
            script.push_str(&format!(" CREATE ledger:'a{n:04}' = {{ n: {n} }};"));
        }
        db.session().run(&script).unwrap();
        let table = {
            let mut transaction = db.store().begin().unwrap();
            Catalog::new(&mut transaction)
                .table_id(NamespaceId::new(1), DatabaseId::new(1), "ledger")
                .unwrap()
                .unwrap()
        };
        let db = Arc::new(db);
        let (address, handle) = door(&authority, &db, Some(Reach::Store), 1 << 20, 6);
        let moved = |first: Gather| -> (usize, usize) {
            let mut gather = first;
            let (mut bytes, mut records) = (0, 0);
            loop {
                let page = ask(&authority, address, &gather).unwrap();
                bytes += page.encode().len();
                records += page.records.len();
                if !page.more {
                    return (bytes, records);
                }
                gather.after = page
                    .resume
                    .clone()
                    .or_else(|| page.records.last().map(|(id, _)| id.clone()));
            }
        };
        let whole = moved(asking(table, 1));
        let bounded = moved(Gather {
            enough: Some(3),
            ..asking(table, 1)
        });
        let narrowed = moved(Gather {
            pushed: Some(narrowed("(n >= $p0)", 1095, None)),
            ..asking(table, 1)
        });
        // ADR-0097 D2: `count(*)` and `sum(n)` over the same shard, folded.
        let folded = moved(Gather {
            reduce: Some(tessari_session::Reduce {
                visible: None,
                condition: None,
                keys: Vec::new(),
                folds: vec![
                    tessari_session::Folded::named("count", None).unwrap(),
                    tessari_session::Folded::named(
                        "sum",
                        Some(("n".to_owned(), tessari_session::Parameters::new())),
                    )
                    .unwrap(),
                ],
            }),
            ..asking(table, 1)
        });
        handle.join().unwrap();
        eprintln!(
            "GATHER-BYTES whole={whole:?} bounded={bounded:?} narrowed={narrowed:?} \
             folded={folded:?}"
        );
        assert_eq!((whole.1, bounded.1, narrowed.1, folded.1), (1100, 3, 5, 0));
        assert!(
            bounded.0 * 100 < whole.0 && narrowed.0 * 100 < whole.0 && folded.0 * 100 < whole.0,
            "{whole:?} {bounded:?} {narrowed:?} {folded:?}"
        );
    }
}
