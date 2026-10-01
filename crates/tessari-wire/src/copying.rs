//! A follower behind a pruned log copies its leader's state, then follows
//! (ADR-0094 D3).
//!
//! One connection carries the whole copy, as a base backup streams: a head
//! naming the reach, the version and where each log stood at it; the state in
//! chunks; an end with the counts and the topic heads. The leader's read
//! transaction and its prune hold live as long as the connection, so a follower
//! that dies mid-copy holds nothing.
//!
//! The follower installs over what it holds — the leader's records rewrite
//! theirs, then every record the copy did not rewrite is removed — so its
//! in-memory catalog stays the store's own throughout (amendment 4').

use std::time::Duration;

use rustls::pki_types::CertificateDer;
use tessari_encoding::{LogId, LogRecord, NODE_ID_LEN, StoreValue};
use tessari_storage::{Store, TopicHead};
use tessari_types::{DatabaseId, NamespaceId, Reach, Sequence, TableId};

use crate::error::{Error, Result};
use crate::frame;
use crate::link::{Credential, hear, open, say};
use crate::peer::{Hello, PeerFrame};

/// How many records one chunk frame carries — few enough that a chunk of large
/// files stays far inside the frame ceiling.
const COPY_CHUNK: usize = 64;

/// What one copy installed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Copied {
    /// The reach the leader served the copy under.
    pub over: Reach,
    /// The leader's version the state was read at.
    pub version: Sequence,
    /// Where each copied log stands now on this node.
    pub positions: Vec<(LogId, Sequence)>,
    /// How many records the copy carried.
    pub records: u64,
    /// How many records this node held that the leader no longer does.
    pub removed: u64,
}

/// Stream `store`'s state, as a follower subscribed at `over` is given it.
///
/// # Errors
///
/// The store's failure, and whatever `write` returns.
pub(crate) fn serve(
    store: &Store,
    over: Reach,
    write: &mut dyn FnMut(u8, Vec<u8>) -> Result<()>,
) -> Result<()> {
    let refused = |why: tessari_storage::Error| Error::Refused {
        message: why.to_string(),
    };
    let mut reader = store.read_state_within(over).map_err(refused)?;
    let positions = reader.positions().to_vec();
    // Taken before the first byte leaves, released the moment this returns
    // early — a copy that failed is one nobody will collect after.
    let hold = store.hold_logs(&positions);
    let mut head = Vec::new();
    frame::put_reach(&mut head, over);
    frame::put_u64(&mut head, reader.version().get());
    frame::put_u32(
        &mut head,
        u32::try_from(positions.len()).unwrap_or(u32::MAX),
    );
    for (log, at) in &positions {
        frame::put_log(&mut head, *log);
        frame::put_u64(&mut head, at.get());
    }
    write(PeerFrame::StateHead.tag(), head)?;
    let mut records = 0_u64;
    while let Some(chunk) = reader.next_chunk(COPY_CHUNK).map_err(refused)? {
        records = records.saturating_add(u64::try_from(chunk.mutations().len()).unwrap_or(0));
        write(
            PeerFrame::StateChunk.tag(),
            chunk.encode().as_slice().to_vec(),
        )?;
    }
    let topics = reader.topic_heads().map_err(refused)?;
    let mut end = Vec::new();
    frame::put_u64(&mut end, records);
    frame::put_u32(&mut end, u32::try_from(topics.len()).unwrap_or(u32::MAX));
    for topic in &topics {
        frame::put_u32(&mut end, topic.namespace.get());
        frame::put_u32(&mut end, topic.database.get());
        frame::put_u32(&mut end, topic.table.get());
        frame::put_u64(&mut end, topic.last);
    }
    write(PeerFrame::StateEnd.tag(), end)?;
    hold.keep_for(Duration::from_secs(
        tessari_constants::REPLICA_COPY_GRACE_SECONDS,
    ));
    Ok(())
}

/// Copy the state the peer `at` holds for this node's subscription into
/// `into`, and stand each copied log where the leader's stood.
///
/// # Errors
///
/// [`Error::Unsubscribed`] when the leader grants this node nothing, a
/// transport or framing failure, and the store's refusal when a chunk or the
/// sweep cannot land. A failure leaves `into` behind its leader, never ahead
/// of it: the positions are written last.
pub fn copy(
    address: &str,
    mine: Credential,
    authority: &CertificateDer<'_>,
    at: [u8; NODE_ID_LEN],
    said: &Hello,
    into: &Store,
) -> Result<Copied> {
    let (mut session, mut socket) = open(address, mine, authority, at)?;
    let copied = {
        let mut link = rustls::Stream::new(&mut session, &mut socket);
        say(&mut link, said)?;
        hear(&mut link)?;
        frame::write_tagged(&mut link, PeerFrame::State.tag(), &[])?;
        receive(&mut link, into)
    };
    session.send_close_notify();
    drop(session.write_tls(&mut socket));
    copied
}

/// Read a copy off `link` and install it.
fn receive(link: &mut impl std::io::Read, into: &Store) -> Result<Copied> {
    let refused = |why: tessari_storage::Error| Error::Refused {
        message: why.to_string(),
    };
    let (over, version, positions) = match next(link)? {
        (tag, body) if tag == PeerFrame::StateHead.tag() => head(&body)?,
        (tag, _) if tag == PeerFrame::Unsubscribed.tag() => return Err(Error::Unsubscribed),
        (tag, _) => return Err(Error::OutOfTurn { tag }),
    };
    let before = into.committed_version().map_err(refused)?;
    let mut records = 0_u64;
    loop {
        let (tag, body) = next(link)?;
        if tag == PeerFrame::StateChunk.tag() {
            let chunk = LogRecord::decode(&body).map_err(|_| Error::Malformed)?;
            records = records.saturating_add(u64::try_from(chunk.mutations().len()).unwrap_or(0));
            into.restore_state_chunk(&chunk).map_err(refused)?;
        } else if tag == PeerFrame::StateEnd.tag() {
            let (carried, topics) = end(&body)?;
            if carried != records {
                // A count that disagrees is a copy that lost a chunk on the way:
                // standing the logs at the leader's positions now would claim a
                // state this node does not hold.
                return Err(Error::Malformed);
            }
            let removed = into.sweep_unreplaced(over, before).map_err(refused)?;
            into.finish_state(&positions, &topics).map_err(refused)?;
            return Ok(Copied {
                over,
                version,
                positions,
                records,
                removed,
            });
        } else {
            return Err(Error::OutOfTurn { tag });
        }
    }
}

fn next(link: &mut impl std::io::Read) -> Result<(u8, Vec<u8>)> {
    frame::read_tagged(link)?.ok_or(Error::Truncated)
}

type Head = (Reach, Sequence, Vec<(LogId, Sequence)>);

fn head(body: &[u8]) -> Result<Head> {
    let (over, at) = frame::take_reach(body, 0)?;
    let (version, at) = frame::take_u64(body, at)?;
    let (count, mut at) = frame::take_u32(body, at)?;
    let mut positions = Vec::new();
    for _ in 0..count {
        let (log, after_log) = frame::take_log(body, at)?;
        let (position, after) = frame::take_u64(body, after_log)?;
        positions.push((log, Sequence::new(position)));
        at = after;
    }
    Ok((over, Sequence::new(version), positions))
}

fn end(body: &[u8]) -> Result<(u64, Vec<TopicHead>)> {
    let (records, at) = frame::take_u64(body, 0)?;
    let (count, mut at) = frame::take_u32(body, at)?;
    let mut topics = Vec::new();
    for _ in 0..count {
        let (namespace, next_at) = frame::take_u32(body, at)?;
        let (database, next_at) = frame::take_u32(body, next_at)?;
        let (table, next_at) = frame::take_u32(body, next_at)?;
        let (last, next_at) = frame::take_u64(body, next_at)?;
        topics.push(TopicHead {
            namespace: NamespaceId::new(namespace),
            database: DatabaseId::new(database),
            table: TableId::new(table),
            last,
        });
        at = next_at;
    }
    Ok((records, topics))
}
