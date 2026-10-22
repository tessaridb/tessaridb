//! A snapshot read one frame at a time.
//!
//! The reader is pulled rather than handed a callback, so the same checks serve
//! a verification that applies nothing and a restore that awaits the store for
//! every chunk.

use std::io::Read;

use tessari_encoding::{LogId, LogRecord, NodeVersion, StoreValue};
use tessari_encoding::{StoreKey, TopicMark, TopicOffsetKey};
use tessari_storage::{TopicEntry, TopicHead};
use tessari_types::{DatabaseId, NamespaceId, Sequence, TableId};

use super::{OLDEST_STATE_FORMAT, STATE_FORMAT, StateTaken, exact, is_state};
use crate::format::Head;
use crate::{Error, FRAME_SECTION, FRAME_SHARD_SECTION, Result, check, log_in, shard_log_in};

use super::{FRAME_CHUNK, FRAME_END, FRAME_ENTRY, FRAME_TOPIC};

/// One thing a snapshot carries after its head.
pub(super) enum Item {
    Chunk(LogRecord),
    Topic(TopicHead),
    Entry(TopicEntry),
}

/// What the next read of a snapshot found.
pub(super) enum Frame {
    /// A chunk or a topic, checked.
    Item(Item),
    /// The end frame, agreeing with everything read before it.
    End(StateTaken),
}

/// A snapshot whose head has been read and checked, read on frame by frame.
pub(super) struct Frames<'a, R> {
    input: &'a mut R,
    format: u8,
    writer: NodeVersion,
    version: Sequence,
    positions: Vec<(LogId, Sequence)>,
    chunks: u64,
    records: u64,
    topics: usize,
    entries: u64,
}

impl<'a, R: Read> Frames<'a, R> {
    /// Read and check the head: the magic, the format, the codec, the writer
    /// and where each log stood.
    pub(super) fn open(input: &'a mut R) -> Result<Self> {
        let mut magic = [0_u8; 11];
        input
            .read_exact(&mut magic)
            .map_err(|_| Error::NotABackup)?;
        if !is_state(&magic) {
            return Err(Error::NotABackup);
        }
        let [format, codec] = exact::<2>(input)?;
        if !(OLDEST_STATE_FORMAT..=STATE_FORMAT).contains(&format) {
            return Err(Error::Unsupported {
                what: "snapshot format",
                found: format,
                supported: STATE_FORMAT,
            });
        }
        if codec != tessari_encoding::CODEC_VERSION {
            return Err(Error::Unsupported {
                what: "record codec",
                found: codec,
                supported: tessari_encoding::CODEC_VERSION,
            });
        }
        let writer = Head::writer(input)?;
        let running = NodeVersion::current();
        if writer > running {
            return Err(Error::WrittenByNewer {
                found: writer.to_string(),
                supported: running.to_string(),
            });
        }
        let version = Sequence::new(u64::from_be_bytes(exact::<8>(input)?));
        let logs = u32::from_be_bytes(exact::<4>(input)?);
        let mut positions = Vec::new();
        for _ in 0..logs {
            let log = match exact::<1>(input)? {
                [FRAME_SECTION] => log_in(exact::<25>(input)?),
                [FRAME_SHARD_SECTION] => shard_log_in(exact::<33>(input)?),
                _ => None,
            }
            .ok_or(Error::NotABackup)?;
            let at = Sequence::new(u64::from_be_bytes(exact::<8>(input)?));
            positions.push((log, at));
        }
        Ok(Self {
            input,
            format,
            writer,
            version,
            positions,
            chunks: 0,
            records: 0,
            topics: 0,
            entries: 0,
        })
    }

    /// Whether the file carries its topics' entries — format 2 on.
    pub(super) const fn carries_entries(&self) -> bool {
        self.format >= 2
    }

    /// Read the next frame, refusing one that is damaged, cut short or not a
    /// snapshot's.
    pub(super) fn next(&mut self) -> Result<Frame> {
        let input = &mut *self.input;
        match exact::<1>(input) {
            Ok([FRAME_CHUNK]) => {
                let length = u32::from_be_bytes(exact::<4>(input)?);
                let sum = u32::from_be_bytes(exact::<4>(input)?);
                // Read through `take` rather than into a buffer of the stated
                // length: a damaged length would otherwise allocate up to four
                // gigabytes before the checksum could refuse the frame, while
                // this grows only as far as the file actually goes.
                let mut body = Vec::new();
                input
                    .by_ref()
                    .take(u64::from(length))
                    .read_to_end(&mut body)?;
                if u64::try_from(body.len()).unwrap_or(u64::MAX) != u64::from(length) {
                    return Err(Error::StateIncomplete);
                }
                if check::crc32(&body) != sum {
                    return Err(Error::Damaged {
                        sequence: self.chunks,
                    });
                }
                let chunk = LogRecord::decode(&body)?;
                let held = u64::try_from(chunk.mutations().len()).unwrap_or(u64::MAX);
                self.records = self.records.saturating_add(held);
                self.chunks = self.chunks.saturating_add(1);
                Ok(Frame::Item(Item::Chunk(chunk)))
            }
            Ok([FRAME_TOPIC]) => {
                let namespace = NamespaceId::new(u32::from_be_bytes(exact::<4>(input)?));
                let database = DatabaseId::new(u32::from_be_bytes(exact::<4>(input)?));
                let table = TableId::new(u32::from_be_bytes(exact::<4>(input)?));
                let last = u64::from_be_bytes(exact::<8>(input)?);
                self.topics = self.topics.saturating_add(1);
                Ok(Frame::Item(Item::Topic(TopicHead {
                    namespace,
                    database,
                    table,
                    last,
                })))
            }
            Ok([FRAME_ENTRY]) if self.format >= 2 => {
                let key_length = u16::from_be_bytes(exact::<2>(input)?);
                let key = bounded(input, usize::from(key_length))?;
                let [mark_length] = exact::<1>(input)?;
                let mark = bounded(input, usize::from(mark_length))?;
                self.entries = self.entries.saturating_add(1);
                Ok(Frame::Item(Item::Entry(TopicEntry {
                    key: TopicOffsetKey::decode(&key)?,
                    mark: TopicMark::decode(&mark)?,
                })))
            }
            Ok([FRAME_END]) => {
                // Format 1 counts chunks and records; format 2 entries as well.
                let mut end = exact::<16>(input)?.to_vec();
                if self.format >= 2 {
                    end.extend_from_slice(&exact::<8>(input)?);
                }
                let sum = u32::from_be_bytes(exact::<4>(input)?);
                let mut counts = self.chunks.to_be_bytes().to_vec();
                counts.extend_from_slice(&self.records.to_be_bytes());
                if self.format >= 2 {
                    counts.extend_from_slice(&self.entries.to_be_bytes());
                }
                let whole = check::crc32(&end) == sum && end == counts;
                if !whole {
                    return Err(Error::StateIncomplete);
                }
                Ok(Frame::End(StateTaken {
                    writer: self.writer,
                    version: self.version,
                    positions: std::mem::take(&mut self.positions),
                    records: self.records,
                    topics: self.topics,
                    entries: self.entries,
                }))
            }
            Ok(_) => Err(Error::NotABackup),
            Err(_) => Err(Error::StateIncomplete),
        }
    }
}

/// Exactly `length` bytes, or the file ended before its end frame.
fn bounded(input: &mut impl Read, length: usize) -> Result<Vec<u8>> {
    let mut bytes = vec![0_u8; length];
    match crate::format::fill(input, &mut bytes)? {
        crate::Filled::Whole => Ok(bytes),
        crate::Filled::Empty | crate::Filled::Short => Err(Error::StateIncomplete),
    }
}
