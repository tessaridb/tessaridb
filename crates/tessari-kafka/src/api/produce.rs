//! Produce: records into a topic's partitions.

use crate::codec::{Reader, Writer};
use crate::error::Result;

/// One partition's records in a produce request, still encoded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PartitionData<'a> {
    /// The partition.
    pub index: i32,
    /// Its record batches; decoded only once the topic is known to be writable.
    pub records: Option<&'a [u8]>,
}

/// One topic of a produce request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TopicData<'a> {
    /// The topic's name as the client wrote it.
    pub name: &'a str,
    /// Its partitions.
    pub partitions: Vec<PartitionData<'a>>,
}

/// A produce request, from version 3.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Request<'a> {
    /// Set only by a transactional producer, which this node refuses.
    pub transactional_id: Option<&'a str>,
    /// 0 (answer nothing), 1 or −1 (answer after the write).
    pub acks: i16,
    /// How long the client waits.
    pub timeout_ms: i32,
    /// The topics.
    pub topics: Vec<TopicData<'a>>,
}

impl<'a> Request<'a> {
    /// Read the body.
    ///
    /// # Errors
    ///
    /// What the reader refuses.
    pub fn read(reader: &mut Reader<'a>, flexible: bool) -> Result<Self> {
        let transactional_id = reader.nullable_string(flexible, "transactional_id")?;
        let acks = reader.i16("acks")?;
        let timeout_ms = reader.i32("timeout_ms")?;
        let count = reader.array_len(flexible, "topic_data")?.unwrap_or(0);
        let mut topics = Vec::with_capacity(count);
        for _ in 0..count {
            let name = reader.string(flexible, "topic name")?;
            let count = reader.array_len(flexible, "partition_data")?.unwrap_or(0);
            let mut partitions = Vec::with_capacity(count);
            for _ in 0..count {
                let index = reader.i32("partition index")?;
                let records = reader.nullable_bytes(flexible, "records")?;
                if flexible {
                    reader.tagged_fields("partition data")?;
                }
                partitions.push(PartitionData { index, records });
            }
            if flexible {
                reader.tagged_fields("topic data")?;
            }
            topics.push(TopicData { name, partitions });
        }
        if flexible {
            reader.tagged_fields("produce request")?;
        }
        Ok(Self {
            transactional_id,
            acks,
            timeout_ms,
            topics,
        })
    }
}

/// How one partition's write went.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PartitionResponse {
    /// The partition.
    pub index: i32,
    /// The protocol's error code, 0 for none.
    pub error_code: i16,
    /// The offset the first record took.
    pub base_offset: i64,
    /// The topic's first offset still held.
    pub log_start_offset: i64,
    /// What went wrong, in the store's words.
    pub error_message: Option<String>,
}

/// One topic's answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TopicResponse {
    /// The name as the client wrote it.
    pub name: String,
    /// Its partitions.
    pub partitions: Vec<PartitionResponse>,
}

/// Write the produce response at `version`.
///
/// # Errors
///
/// What the writer refuses.
pub fn write_response(
    writer: &mut Writer,
    version: i16,
    flexible: bool,
    topics: &[TopicResponse],
) -> Result<()> {
    writer.array_len(flexible, Some(topics.len()))?;
    for topic in topics {
        writer.string(flexible, &topic.name)?;
        writer.array_len(flexible, Some(topic.partitions.len()))?;
        for partition in &topic.partitions {
            writer.i32(partition.index);
            writer.i16(partition.error_code);
            writer.i64(partition.base_offset);
            writer.i64(-1);
            if version >= 5 {
                writer.i64(partition.log_start_offset);
            }
            if version >= 8 {
                writer.array_len(flexible, Some(0))?;
                writer.nullable_string(flexible, partition.error_message.as_deref())?;
            }
            if flexible {
                writer.no_tagged_fields();
            }
        }
        if flexible {
            writer.no_tagged_fields();
        }
    }
    writer.i32(0);
    if flexible {
        writer.no_tagged_fields();
    }
    Ok(())
}
