//! Fetch: messages of a partition from an offset.
//!
//! Fetch sessions (incremental fetch) are not created: every answer carries
//! session id 0, which tells a client to send the whole request each time.

use crate::codec::{Reader, Writer};
use crate::error::Result;

/// One partition to fetch from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Partition {
    /// The partition.
    pub index: i32,
    /// The offset of the first message wanted.
    pub fetch_offset: i64,
    /// The most bytes this partition's records may take in the answer.
    pub max_bytes: i32,
}

/// One topic to fetch from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Topic<'a> {
    /// Its name.
    pub name: &'a str,
    /// Its partitions.
    pub partitions: Vec<Partition>,
}

/// A fetch request, from version 4.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Request<'a> {
    /// How long to wait for `min_bytes` before answering with what there is.
    pub max_wait_ms: i32,
    /// The least bytes worth answering with before the wait is over.
    pub min_bytes: i32,
    /// The most bytes the whole answer may take.
    pub max_bytes: i32,
    /// The topics.
    pub topics: Vec<Topic<'a>>,
}

impl<'a> Request<'a> {
    /// Read the body at `version`.
    ///
    /// # Errors
    ///
    /// What the reader refuses.
    pub fn read(reader: &mut Reader<'a>, version: i16, flexible: bool) -> Result<Self> {
        let _replica = reader.i32("replica_id")?;
        let max_wait_ms = reader.i32("max_wait_ms")?;
        let min_bytes = reader.i32("min_bytes")?;
        let max_bytes = if version >= 3 {
            reader.i32("max_bytes")?
        } else {
            i32::MAX
        };
        let _isolation = reader.i8("isolation_level")?;
        if version >= 7 {
            let _session = reader.i32("session_id")?;
            let _epoch = reader.i32("session_epoch")?;
        }
        let count = reader.array_len(flexible, "topics")?.unwrap_or(0);
        let mut topics = Vec::with_capacity(count);
        for _ in 0..count {
            let name = reader.string(flexible, "topic")?;
            let count = reader.array_len(flexible, "partitions")?.unwrap_or(0);
            let mut partitions = Vec::with_capacity(count);
            for _ in 0..count {
                let index = reader.i32("partition")?;
                if version >= 9 {
                    let _epoch = reader.i32("current_leader_epoch")?;
                }
                let fetch_offset = reader.i64("fetch_offset")?;
                if version >= 12 {
                    let _last = reader.i32("last_fetched_epoch")?;
                }
                if version >= 5 {
                    let _start = reader.i64("log_start_offset")?;
                }
                let max_bytes = reader.i32("partition_max_bytes")?;
                if flexible {
                    reader.tagged_fields("fetch partition")?;
                }
                partitions.push(Partition {
                    index,
                    fetch_offset,
                    max_bytes,
                });
            }
            if flexible {
                reader.tagged_fields("fetch topic")?;
            }
            topics.push(Topic { name, partitions });
        }
        if version >= 7 {
            let count = reader
                .array_len(flexible, "forgotten_topics_data")?
                .unwrap_or(0);
            for _ in 0..count {
                reader.string(flexible, "forgotten topic")?;
                let count = reader
                    .array_len(flexible, "forgotten partitions")?
                    .unwrap_or(0);
                for _ in 0..count {
                    reader.i32("forgotten partition")?;
                }
                if flexible {
                    reader.tagged_fields("forgotten topic")?;
                }
            }
        }
        if version >= 11 {
            reader.string(flexible, "rack_id")?;
        }
        if flexible {
            reader.tagged_fields("fetch request")?;
        }
        Ok(Self {
            max_wait_ms,
            min_bytes,
            max_bytes,
            topics,
        })
    }
}

/// One partition's answer, its records already encoded as batches.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PartitionAnswer {
    /// The partition.
    pub index: i32,
    /// The protocol's error code.
    pub error_code: i16,
    /// The offset the next message will take.
    pub high_watermark: i64,
    /// The first offset still held.
    pub log_start_offset: i64,
    /// The record batches, or `None` with an error.
    pub records: Option<Vec<u8>>,
}

/// Write the response at `version`.
///
/// # Errors
///
/// What the writer refuses.
pub fn write_response(
    writer: &mut Writer,
    version: i16,
    flexible: bool,
    topics: &[(String, Vec<PartitionAnswer>)],
) -> Result<()> {
    writer.i32(0);
    if version >= 7 {
        writer.i16(0);
        writer.i32(0);
    }
    writer.array_len(flexible, Some(topics.len()))?;
    for (name, partitions) in topics {
        writer.string(flexible, name)?;
        writer.array_len(flexible, Some(partitions.len()))?;
        for partition in partitions {
            writer.i32(partition.index);
            writer.i16(partition.error_code);
            writer.i64(partition.high_watermark);
            writer.i64(partition.high_watermark);
            if version >= 5 {
                writer.i64(partition.log_start_offset);
            }
            writer.array_len(flexible, Some(0))?;
            if version >= 11 {
                writer.i32(-1);
            }
            writer.nullable_bytes(flexible, partition.records.as_deref())?;
            if flexible {
                writer.no_tagged_fields();
            }
        }
        if flexible {
            writer.no_tagged_fields();
        }
    }
    if flexible {
        writer.no_tagged_fields();
    }
    Ok(())
}
