//! ListOffsets: the first and next offsets of a partition, or the one at a time.

use crate::codec::{Reader, Writer};
use crate::error::Result;

/// Ask for the offset of the first message held.
pub const EARLIEST: i64 = -2;
/// Ask for the offset the next message will take.
pub const LATEST: i64 = -1;
/// Ask for the offset of the message with the highest timestamp (version 7).
pub const MAX_TIMESTAMP: i64 = -3;

/// One partition asked about.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Partition {
    /// The partition.
    pub index: i32,
    /// A timestamp in milliseconds, or one of the three markers.
    pub timestamp: i64,
}

/// One topic asked about.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Topic<'a> {
    /// Its name.
    pub name: &'a str,
    /// Its partitions.
    pub partitions: Vec<Partition>,
}

/// Read the body at `version`.
///
/// # Errors
///
/// What the reader refuses.
pub fn read_request<'a>(
    reader: &mut Reader<'a>,
    version: i16,
    flexible: bool,
) -> Result<Vec<Topic<'a>>> {
    let _replica = reader.i32("replica_id")?;
    if version >= 2 {
        let _isolation = reader.i8("isolation_level")?;
    }
    let count = reader.array_len(flexible, "topics")?.unwrap_or(0);
    let mut topics = Vec::with_capacity(count);
    for _ in 0..count {
        let name = reader.string(flexible, "topic name")?;
        let count = reader.array_len(flexible, "partitions")?.unwrap_or(0);
        let mut partitions = Vec::with_capacity(count);
        for _ in 0..count {
            let index = reader.i32("partition_index")?;
            if version >= 4 {
                let _epoch = reader.i32("current_leader_epoch")?;
            }
            let timestamp = reader.i64("timestamp")?;
            if flexible {
                reader.tagged_fields("list offsets partition")?;
            }
            partitions.push(Partition { index, timestamp });
        }
        if flexible {
            reader.tagged_fields("list offsets topic")?;
        }
        topics.push(Topic { name, partitions });
    }
    if flexible {
        reader.tagged_fields("list offsets request")?;
    }
    Ok(topics)
}

/// One partition's answer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Answer {
    /// The partition.
    pub index: i32,
    /// The protocol's error code.
    pub error_code: i16,
    /// The timestamp of the offset found, −1 for a marker.
    pub timestamp: i64,
    /// The offset, −1 when none matches.
    pub offset: i64,
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
    topics: &[(String, Vec<Answer>)],
) -> Result<()> {
    if version >= 2 {
        writer.i32(0);
    }
    writer.array_len(flexible, Some(topics.len()))?;
    for (name, partitions) in topics {
        writer.string(flexible, name)?;
        writer.array_len(flexible, Some(partitions.len()))?;
        for partition in partitions {
            writer.i32(partition.index);
            writer.i16(partition.error_code);
            writer.i64(partition.timestamp);
            writer.i64(partition.offset);
            if version >= 4 {
                writer.i32(-1);
            }
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
