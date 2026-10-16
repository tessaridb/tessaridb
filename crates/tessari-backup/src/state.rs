//! A backup of the store's current state rather than of its log (ADR-0091).
//!
//! # The format
//!
//! ```text
//! head     "TESSARISNAP" <format:u8> <codec:u8> <writer:u32*3> <version:u64> <logs:u32>
//!          then <logs> × (<tag:u8> <log:25|33> <position:u64>)
//! frame    <tag:u8>
//!   tag 2  chunk   <length:u32> <crc32:u32> <a log record of up to CHUNK mutations>
//!   tag 4  topic   <namespace:u32> <database:u32> <table:u32> <last:u64>
//!   tag 5  end     <chunks:u64> <records:u64> <crc32 of the sixteen bytes before it:u32>
//! ```
//!
//! A chunk is written in the log record codec, because a chunk IS a set of
//! mutations and the codec already says how to carry one, versions and all. The
//! positions are the ones the state was read at, per log, so a log backup taken
//! `FROM` the next position continues where this file stops.
//!
//! # Why a snapshot is refused whole where a log is restored in part
//!
//! A cut log is a prefix of history and a prefix is a state the store once held.
//! A cut snapshot is an arbitrary subset of records — the tables that happened to
//! sort first — and no store ever held it. So nothing is taken from a file whose
//! end frame is missing or disagrees with what was read: the end frame counts the
//! chunks and records, which also catches a chunk dropped from the middle, and
//! every chunk carries its own checksum.

use std::io::{Read, Write};

use tessari_encoding::{LogId, NodeVersion, StoreValue};
use tessari_storage::Store;
use tessari_types::{Reach, Sequence};

use crate::format::fill;

mod frames;
use crate::{
    Error, FRAME_SECTION, FRAME_SHARD_SECTION, Filled, Result, check, log_bytes, shard_log_bytes,
};
use frames::{Frame, Frames, Item};

/// What every snapshot file begins with.
pub const STATE_MAGIC: &[u8; 11] = b"TESSARISNAP";

/// The snapshot format's own version.
const STATE_FORMAT: u8 = 1;

/// How many records one chunk holds.
const CHUNK: usize = 500;

const FRAME_CHUNK: u8 = 2;
const FRAME_TOPIC: u8 = 4;
const FRAME_END: u8 = 5;

/// What a snapshot holds, as writing or reading it found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StateTaken {
    /// The build that wrote the file.
    pub writer: NodeVersion,
    /// The store version the state was read at.
    pub version: Sequence,
    /// Where each log stood at that version.
    pub positions: Vec<(LogId, Sequence)>,
    /// How many records it carries.
    pub records: u64,
    /// How many topics' last positions it carries.
    pub topics: usize,
}

/// Whether a file beginning with `opening` is a snapshot rather than a log.
#[must_use]
pub fn is_state(opening: &[u8]) -> bool {
    opening.starts_with(STATE_MAGIC)
}

/// Write the state of `store` to `out`.
///
/// # Errors
///
/// Returns an error when the store or the stream fails.
pub async fn write_state(store: &Store, out: &mut impl Write) -> Result<StateTaken> {
    write_state_within(store, Reach::Store, out).await
}

/// Write the state of one place in `store` — a namespace or a database — to
/// `out` (ADR-0094 D5).
///
/// It is what a follower subscribed at `within` is given: the place's records,
/// the catalog that defines it, and the logs inside or above it. The format is
/// the whole store's, so it verifies and restores the same way, into an empty
/// store.
///
/// # Errors
///
/// Returns an error when the store or the stream fails.
pub async fn write_state_within(
    store: &Store,
    within: Reach,
    out: &mut impl Write,
) -> Result<StateTaken> {
    let mut reader = store.read_state_within(within).await?;
    let writer = NodeVersion::current();
    out.write_all(STATE_MAGIC)?;
    out.write_all(&[STATE_FORMAT, tessari_encoding::CODEC_VERSION])?;
    out.write_all(&writer.major.to_be_bytes())?;
    out.write_all(&writer.minor.to_be_bytes())?;
    out.write_all(&writer.patch.to_be_bytes())?;
    out.write_all(&reader.version().get().to_be_bytes())?;
    let positions = reader.positions().to_vec();
    let logs = u32::try_from(positions.len()).unwrap_or(u32::MAX);
    out.write_all(&logs.to_be_bytes())?;
    for (log, at) in &positions {
        if let Some(named) = shard_log_bytes(*log) {
            out.write_all(&[FRAME_SHARD_SECTION])?;
            out.write_all(&named)?;
        } else {
            out.write_all(&[FRAME_SECTION])?;
            out.write_all(&log_bytes(*log))?;
        }
        out.write_all(&at.get().to_be_bytes())?;
    }
    let mut chunks = 0_u64;
    let mut records = 0_u64;
    while let Some(chunk) = reader.next_chunk(CHUNK).await? {
        let bytes = chunk.encode();
        let body = bytes.as_slice();
        let length = u32::try_from(body.len()).unwrap_or(u32::MAX);
        out.write_all(&[FRAME_CHUNK])?;
        out.write_all(&length.to_be_bytes())?;
        out.write_all(&check::crc32(body).to_be_bytes())?;
        out.write_all(body)?;
        chunks = chunks.saturating_add(1);
        let held = u64::try_from(chunk.mutations().len()).unwrap_or(u64::MAX);
        records = records.saturating_add(held);
    }
    let topics = reader.topic_heads().await?;
    for topic in &topics {
        out.write_all(&[FRAME_TOPIC])?;
        out.write_all(&topic.namespace.get().to_be_bytes())?;
        out.write_all(&topic.database.get().to_be_bytes())?;
        out.write_all(&topic.table.get().to_be_bytes())?;
        out.write_all(&topic.last.to_be_bytes())?;
    }
    let mut end = [0_u8; 16];
    end[..8].copy_from_slice(&chunks.to_be_bytes());
    end[8..].copy_from_slice(&records.to_be_bytes());
    out.write_all(&[FRAME_END])?;
    out.write_all(&end)?;
    out.write_all(&check::crc32(&end).to_be_bytes())?;
    Ok(StateTaken {
        writer,
        version: reader.version(),
        positions,
        records,
        topics: topics.len(),
    })
}

/// Read a snapshot without applying it, and say what it holds.
///
/// # Errors
///
/// The refusals [`read_state`] gives before applying anything, and
/// [`Error::StateIncomplete`] or [`Error::Damaged`] for a file that is not whole.
pub fn verify_state(input: &mut impl Read) -> Result<StateTaken> {
    let mut frames = Frames::open(input)?;
    loop {
        if let Frame::End(taken) = frames.next()? {
            return Ok(taken);
        }
    }
}

/// Restore a snapshot into an **empty** store.
///
/// The whole file is checked before the first record is written, because a
/// snapshot is taken whole or not at all — so `open` is called twice and the
/// file read twice, which is the price of never leaving half a state behind a
/// refusal.
///
/// A transaction across leaders whose outcome the snapshot's cut decided is
/// finished at the end, as nobody else will; one the cut left undecided stays
/// standing and unseen, for the log after the snapshot to finish.
///
/// # Errors
///
/// [`Error::WrongBase`] when the store holds anything, the refusals of
/// [`verify_state`], and the store's own error when a chunk cannot be written.
pub async fn read_state<R: Read>(
    store: &Store,
    mut open: impl FnMut() -> std::io::Result<R>,
) -> Result<StateTaken> {
    if !store.holds_nothing()? {
        return Err(Error::WrongBase {
            needs: 0,
            found: store.committed_version()?.get(),
        });
    }
    verify_state(&mut open()?)?;
    let mut topics = Vec::new();
    let mut input = open()?;
    let mut frames = Frames::open(&mut input)?;
    let taken = loop {
        match frames.next()? {
            Frame::Item(Item::Chunk(chunk)) => store.restore_state_chunk(&chunk).await?,
            Frame::Item(Item::Topic(topic)) => topics.push(topic),
            Frame::End(taken) => break taken,
        }
    };
    store.finish_state(&taken.positions, &topics).await?;
    // Restored on its own, nobody else will finish what the cut decided: the
    // store settles it, leaving the undecided for the log above (Q-922b).
    store.settle_restored().await?;
    Ok(taken)
}

/// Exactly `N` bytes, or the file ended before its end frame.
fn exact<const N: usize>(input: &mut impl Read) -> Result<[u8; N]> {
    let mut bytes = [0_u8; N];
    match fill(input, &mut bytes)? {
        Filled::Whole => Ok(bytes),
        Filled::Empty | Filled::Short => Err(Error::StateIncomplete),
    }
}
