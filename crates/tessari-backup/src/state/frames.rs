//! A snapshot read one frame at a time.
//!
//! The reader is pulled rather than handed a callback, so the same checks serve
//! a verification that applies nothing and a restore that awaits the store for
//! every chunk.

use std::io::Read;

use tessari_encoding::{LogId, LogRecord, NodeVersion, StoreValue};
use tessari_storage::TopicHead;
use tessari_types::{DatabaseId, NamespaceId, Sequence, TableId};

use super::{STATE_FORMAT, StateTaken, exact, is_state};
use crate::format::Head;
use crate::{Error, FRAME_SECTION, FRAME_SHARD_SECTION, Result, check, log_in, shard_log_in};

use super::{FRAME_CHUNK, FRAME_END, FRAME_TOPIC};

/// One thing a snapshot carries after its head.
pub(super) enum Item {
    Chunk(LogRecord),
    Topic(TopicHead),
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
    writer: NodeVersion,
    version: Sequence,
    positions: Vec<(LogId, Sequence)>,
    chunks: u64,
    records: u64,
    topics: usize,
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
        if format != STATE_FORMAT {
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
            writer,
            version,
            positions,
            chunks: 0,
            records: 0,
            topics: 0,
        })
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
            Ok([FRAME_END]) => {
                let end = exact::<16>(input)?;
                let sum = u32::from_be_bytes(exact::<4>(input)?);
                let (counted, held) = end.split_at(8);
                let whole = check::crc32(&end) == sum
                    && counted == self.chunks.to_be_bytes()
                    && held == self.records.to_be_bytes();
                if !whole {
                    return Err(Error::StateIncomplete);
                }
                Ok(Frame::End(StateTaken {
                    writer: self.writer,
                    version: self.version,
                    positions: std::mem::take(&mut self.positions),
                    records: self.records,
                    topics: self.topics,
                }))
            }
            Ok(_) => Err(Error::NotABackup),
            Err(_) => Err(Error::StateIncomplete),
        }
    }
}
