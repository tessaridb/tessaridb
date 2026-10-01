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
}

/// One page of a shard's records.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Page {
    /// Stored records in identity order.
    pub records: Vec<(RecordId, Vec<u8>)>,
    /// Whether the answer stopped at a bound rather than at the window's end.
    pub more: bool,
    /// Where the next page begins when it is not the last record sent: a page
    /// narrowed by a pushed condition may keep none of the records it read.
    pub resume: Option<RecordId>,
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
            body.push(1);
            put_pushed(&mut body, pushed);
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
        let (pushed, at) = match body.get(at) {
            None => (None, at),
            Some(1) => {
                let (pushed, at) = take_pushed(body, next(at)?)?;
                (Some(pushed), at)
            }
            Some(_) => return Err(Error::Malformed),
        };
        if at != body.len() {
            return Err(Error::Malformed);
        }
        Ok(Self {
            pushed,
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
        let (resume, at) = if at == body.len() {
            (None, at)
        } else {
            match take_optional(body, at)? {
                (Some(resume), at) => (Some(resume), at),
                (None, _) => return Err(Error::Malformed),
            }
        };
        if at != body.len() {
            return Err(Error::Malformed);
        }
        Ok(Self {
            records,
            more,
            resume,
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
    budget: usize,
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
    let mut more = found.len() == GATHER_PAGE_RECORDS;
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
    })
}

/// A pushed condition: the visible fields, the text, then each parameter as a
/// name and a value in the store's own codec.
fn put_pushed(into: &mut Vec<u8>, pushed: &tessari_session::Pushed) {
    match &pushed.visible {
        Some(fields) => {
            into.push(1);
            frame::put_u32(into, u32::try_from(fields.len()).unwrap_or(u32::MAX));
            for field in fields {
                frame::put_text(into, field);
            }
        }
        None => into.push(0),
    }
    frame::put_text(into, &pushed.condition);
    frame::put_u32(
        into,
        u32::try_from(pushed.parameters.len()).unwrap_or(u32::MAX),
    );
    for (name, value) in &pushed.parameters {
        frame::put_text(into, name);
        frame::put_bytes(into, &tessari_encoding::encode_payload(value).into_bytes());
    }
}

fn take_pushed(from: &[u8], at: usize) -> Result<(tessari_session::Pushed, usize)> {
    let (visible, mut at) = match from.get(at) {
        Some(0) => (None, next(at)?),
        Some(1) => {
            let (count, mut at) = frame::take_u32(from, next(at)?)?;
            let mut fields = std::collections::BTreeSet::new();
            for _ in 0..count {
                let (field, next_at) = frame::take_text(from, at)?;
                fields.insert(field);
                at = next_at;
            }
            (Some(fields), at)
        }
        _ => return Err(Error::Malformed),
    };
    let (condition, next_at) = frame::take_text(from, at)?;
    at = next_at;
    let (count, next_at) = frame::take_u32(from, at)?;
    at = next_at;
    let mut parameters = tessari_session::Parameters::new();
    for _ in 0..count {
        let (name, next_at) = frame::take_text(from, at)?;
        let (value, next_at) = frame::take_bytes(from, next_at)?;
        let value = tessari_encoding::decode_payload(&value).map_err(|_| Error::Malformed)?;
        parameters.insert(name, value);
        at = next_at;
    }
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

        let page = Page {
            records: vec![
                (RecordId::Bytes(vec![0, 1]), vec![5, 6, 7]),
                (RecordId::Text("h".to_owned()), Vec::new()),
            ],
            more: true,
            resume: None,
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
                    &Serving::within(db.store(), &granting, budget),
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
}
