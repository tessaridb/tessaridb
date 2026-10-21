//! OffsetCommit and OffsetFetch: a group's committed offsets.

use crate::codec::{Reader, Writer};
use crate::error::Result;

/// One partition's offset to commit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Commit {
    /// The partition.
    pub index: i32,
    /// The next offset the group will read.
    pub offset: i64,
}

/// A commit request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommitRequest<'a> {
    /// The group.
    pub group: &'a str,
    /// The committing member's generation, −1 for a commit from outside a group.
    pub generation: i32,
    /// The committing member, empty from outside a group.
    pub member: &'a str,
    /// The topics and their partitions.
    pub topics: Vec<(&'a str, Vec<Commit>)>,
}

impl<'a> CommitRequest<'a> {
    /// Read the body at `version` (2 and later).
    ///
    /// # Errors
    ///
    /// What the reader refuses.
    pub fn read(reader: &mut Reader<'a>, version: i16, flexible: bool) -> Result<Self> {
        let group = reader.string(flexible, "group_id")?;
        let generation = reader.i32("generation_id")?;
        let member = reader.string(flexible, "member_id")?;
        if version >= 7 {
            let _instance = reader.nullable_string(flexible, "group_instance_id")?;
        }
        if version <= 4 {
            let _retention = reader.i64("retention_time_ms")?;
        }
        let count = reader.array_len(flexible, "topics")?.unwrap_or(0);
        let mut topics = Vec::with_capacity(count);
        for _ in 0..count {
            let name = reader.string(flexible, "topic name")?;
            let count = reader.array_len(flexible, "partitions")?.unwrap_or(0);
            let mut partitions = Vec::with_capacity(count);
            for _ in 0..count {
                let index = reader.i32("partition_index")?;
                let offset = reader.i64("committed_offset")?;
                if version >= 6 {
                    let _epoch = reader.i32("committed_leader_epoch")?;
                }
                let _metadata = reader.nullable_string(flexible, "committed_metadata")?;
                if flexible {
                    reader.tagged_fields("offset commit partition")?;
                }
                partitions.push(Commit { index, offset });
            }
            if flexible {
                reader.tagged_fields("offset commit topic")?;
            }
            topics.push((name, partitions));
        }
        if flexible {
            reader.tagged_fields("offset commit")?;
        }
        Ok(Self {
            group,
            generation,
            member,
            topics,
        })
    }
}

/// Write a commit's response: a code per partition.
///
/// # Errors
///
/// What the writer refuses.
pub fn write_commit(
    writer: &mut Writer,
    version: i16,
    flexible: bool,
    topics: &[(String, Vec<(i32, i16)>)],
) -> Result<()> {
    if version >= 3 {
        writer.i32(0);
    }
    writer.array_len(flexible, Some(topics.len()))?;
    for (name, partitions) in topics {
        writer.string(flexible, name)?;
        writer.array_len(flexible, Some(partitions.len()))?;
        for (index, code) in partitions {
            writer.i32(*index);
            writer.i16(*code);
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

/// A fetch of committed offsets.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FetchRequest<'a> {
    /// The group.
    pub group: &'a str,
    /// The topics and partitions asked about, `None` for every topic.
    pub topics: Option<Vec<(&'a str, Vec<i32>)>>,
}

impl<'a> FetchRequest<'a> {
    /// Read the body at `version` (1 to 7).
    ///
    /// # Errors
    ///
    /// What the reader refuses.
    pub fn read(reader: &mut Reader<'a>, version: i16, flexible: bool) -> Result<Self> {
        let group = reader.string(flexible, "group_id")?;
        let topics = match reader.array_len(flexible, "topics")? {
            None => None,
            Some(count) => {
                let mut topics = Vec::with_capacity(count);
                for _ in 0..count {
                    let name = reader.string(flexible, "topic name")?;
                    let count = reader
                        .array_len(flexible, "partition_indexes")?
                        .unwrap_or(0);
                    let mut partitions = Vec::with_capacity(count);
                    for _ in 0..count {
                        partitions.push(reader.i32("partition index")?);
                    }
                    if flexible {
                        reader.tagged_fields("offset fetch topic")?;
                    }
                    topics.push((name, partitions));
                }
                Some(topics)
            }
        };
        if version >= 7 {
            let _stable = reader.bool("require_stable")?;
        }
        if flexible {
            reader.tagged_fields("offset fetch")?;
        }
        Ok(Self { group, topics })
    }
}

/// One partition's committed offset, −1 when none was committed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Committed {
    /// The partition.
    pub index: i32,
    /// The offset.
    pub offset: i64,
    /// The protocol's error code.
    pub error_code: i16,
}

/// Write a fetch's response at `version`.
///
/// # Errors
///
/// What the writer refuses.
pub fn write_fetch(
    writer: &mut Writer,
    version: i16,
    flexible: bool,
    error_code: i16,
    topics: &[(String, Vec<Committed>)],
) -> Result<()> {
    if version >= 3 {
        writer.i32(0);
    }
    writer.array_len(flexible, Some(topics.len()))?;
    for (name, partitions) in topics {
        writer.string(flexible, name)?;
        writer.array_len(flexible, Some(partitions.len()))?;
        for partition in partitions {
            writer.i32(partition.index);
            writer.i64(partition.offset);
            if version >= 5 {
                writer.i32(-1);
            }
            writer.nullable_string(flexible, Some(""))?;
            writer.i16(partition.error_code);
            if flexible {
                writer.no_tagged_fields();
            }
        }
        if flexible {
            writer.no_tagged_fields();
        }
    }
    if version >= 2 {
        writer.i16(error_code);
    }
    if flexible {
        writer.no_tagged_fields();
    }
    Ok(())
}
