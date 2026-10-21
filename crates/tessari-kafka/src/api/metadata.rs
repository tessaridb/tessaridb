//! Metadata: which brokers exist, which topics, and who leads each partition.
//!
//! A node standing alone is the one broker, the controller and the leader of
//! every partition (ADR-0129 D7); on a cluster every member a Kafka client can
//! reach is a broker and a partition is led by the node leading its database.

use crate::codec::{Reader, Writer};
use crate::error::Result;

/// A metadata request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Request<'a> {
    /// The topics asked about; `None` asks for every topic.
    pub topics: Option<Vec<&'a str>>,
    /// Whether the client asked the broker to create a topic it names.
    pub allow_auto_topic_creation: bool,
}

impl<'a> Request<'a> {
    /// Read the body at `version`.
    ///
    /// # Errors
    ///
    /// What the reader refuses.
    pub fn read(reader: &mut Reader<'a>, version: i16, flexible: bool) -> Result<Self> {
        let topics = match reader.array_len(flexible, "topics")? {
            None => None,
            Some(count) => {
                let mut names = Vec::with_capacity(count);
                for _ in 0..count {
                    names.push(reader.string(flexible, "topic name")?);
                    if flexible {
                        reader.tagged_fields("metadata topic")?;
                    }
                }
                Some(names)
            }
        };
        let allow_auto_topic_creation = if version >= 4 {
            reader.bool("allow_auto_topic_creation")?
        } else {
            true
        };
        if version >= 8 {
            if version <= 10 {
                reader.bool("include_cluster_authorized_operations")?;
            }
            reader.bool("include_topic_authorized_operations")?;
        }
        if flexible {
            reader.tagged_fields("metadata request")?;
        }
        Ok(Self {
            topics,
            allow_auto_topic_creation,
        })
    }
}

/// A broker the answer names.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Broker {
    /// Its id.
    pub node_id: i32,
    /// The host a client should connect to.
    pub host: String,
    /// The port.
    pub port: i32,
}

/// One topic in the answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Topic {
    /// The protocol's error code, 0 for a topic that exists.
    pub error_code: i16,
    /// The name as the client asked for it.
    pub name: String,
    /// How many partitions it has (0 when it does not exist).
    pub partitions: i32,
    /// The broker leading its partitions.
    pub leader: i32,
}

/// Write the metadata response at `version`.
///
/// # Errors
///
/// What the writer refuses.
pub fn write_response(
    writer: &mut Writer,
    version: i16,
    flexible: bool,
    brokers: &[Broker],
    controller: i32,
    cluster_id: &str,
    topics: &[Topic],
) -> Result<()> {
    if version >= 3 {
        writer.i32(0);
    }
    writer.array_len(flexible, Some(brokers.len()))?;
    for broker in brokers {
        writer.i32(broker.node_id);
        writer.string(flexible, &broker.host)?;
        writer.i32(broker.port);
        writer.nullable_string(flexible, None)?;
        if flexible {
            writer.no_tagged_fields();
        }
    }
    if version >= 2 {
        writer.nullable_string(flexible, Some(cluster_id))?;
    }
    writer.i32(controller);
    writer.array_len(flexible, Some(topics.len()))?;
    for topic in topics {
        writer.i16(topic.error_code);
        writer.string(flexible, &topic.name)?;
        writer.bool(false);
        let count = usize::try_from(topic.partitions).unwrap_or(0);
        writer.array_len(flexible, Some(count))?;
        for index in 0..topic.partitions.max(0) {
            writer.i16(0);
            writer.i32(index);
            writer.i32(topic.leader);
            if version >= 7 {
                writer.i32(-1);
            }
            writer.array_len(flexible, Some(1))?;
            writer.i32(topic.leader);
            writer.array_len(flexible, Some(1))?;
            writer.i32(topic.leader);
            if version >= 5 {
                writer.array_len(flexible, Some(0))?;
            }
            if flexible {
                writer.no_tagged_fields();
            }
        }
        if version >= 8 {
            writer.i32(i32::MIN);
        }
        if flexible {
            writer.no_tagged_fields();
        }
    }
    if (8..=10).contains(&version) {
        writer.i32(i32::MIN);
    }
    if flexible {
        writer.no_tagged_fields();
    }
    Ok(())
}
