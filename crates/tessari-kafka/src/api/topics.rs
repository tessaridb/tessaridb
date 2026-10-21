//! CreateTopics and DeleteTopics: a topic declared or dropped from a client.

use crate::codec::{Reader, Writer};
use crate::error::Result;

/// One topic a client asks to create.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Creatable<'a> {
    /// Its name.
    pub name: &'a str,
    /// Its partitions, −1 for the broker's default.
    pub partitions: i32,
    /// Its replication factor, −1 for the broker's default.
    pub replication: i16,
    /// Explicit replica assignments: partition and brokers.
    pub assignments: Vec<(i32, Vec<i32>)>,
    /// Its configuration, by name.
    pub configs: Vec<(&'a str, Option<&'a str>)>,
}

/// A create request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CreateRequest<'a> {
    /// The topics.
    pub topics: Vec<Creatable<'a>>,
    /// Check every topic and create none.
    pub validate_only: bool,
}

impl<'a> CreateRequest<'a> {
    /// Read the body at `version` (2 and later).
    ///
    /// # Errors
    ///
    /// What the reader refuses.
    pub fn read(reader: &mut Reader<'a>, flexible: bool) -> Result<Self> {
        let count = reader.array_len(flexible, "topics")?.unwrap_or(0);
        let mut topics = Vec::with_capacity(count);
        for _ in 0..count {
            let name = reader.string(flexible, "topic name")?;
            let partitions = reader.i32("num_partitions")?;
            let replication = reader.i16("replication_factor")?;
            let count = reader.array_len(flexible, "assignments")?.unwrap_or(0);
            let mut assignments = Vec::with_capacity(count);
            for _ in 0..count {
                let index = reader.i32("assignment partition")?;
                let count = reader.array_len(flexible, "broker_ids")?.unwrap_or(0);
                let mut brokers = Vec::with_capacity(count);
                for _ in 0..count {
                    brokers.push(reader.i32("broker id")?);
                }
                if flexible {
                    reader.tagged_fields("create topics assignment")?;
                }
                assignments.push((index, brokers));
            }
            let count = reader.array_len(flexible, "configs")?.unwrap_or(0);
            let mut configs = Vec::with_capacity(count);
            for _ in 0..count {
                let key = reader.string(flexible, "config name")?;
                let value = reader.nullable_string(flexible, "config value")?;
                if flexible {
                    reader.tagged_fields("create topics config")?;
                }
                configs.push((key, value));
            }
            if flexible {
                reader.tagged_fields("create topics topic")?;
            }
            topics.push(Creatable {
                name,
                partitions,
                replication,
                assignments,
                configs,
            });
        }
        let _timeout = reader.i32("timeout_ms")?;
        let validate_only = reader.bool("validate_only")?;
        if flexible {
            reader.tagged_fields("create topics")?;
        }
        Ok(Self {
            topics,
            validate_only,
        })
    }
}

/// One topic's answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Created {
    /// Its name, as asked.
    pub name: String,
    /// The protocol's error code.
    pub error_code: i16,
    /// What was wrong, in words.
    pub message: Option<String>,
}

/// Write a create's response at `version`.
///
/// # Errors
///
/// What the writer refuses.
pub fn write_create(
    writer: &mut Writer,
    version: i16,
    flexible: bool,
    topics: &[Created],
) -> Result<()> {
    writer.i32(0);
    writer.array_len(flexible, Some(topics.len()))?;
    for topic in topics {
        writer.string(flexible, &topic.name)?;
        writer.i16(topic.error_code);
        writer.nullable_string(flexible, topic.message.as_deref())?;
        if version >= 5 {
            let created = topic.error_code == 0;
            writer.i32(if created { 1 } else { -1 });
            writer.i16(if created { 1 } else { -1 });
            // Configuration is not reported back: an empty list, not null, so
            // a client reading it finds nothing rather than an error.
            writer.array_len(flexible, Some(0))?;
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

/// Read a DeleteTopics request (1 to 4): the topics to drop.
///
/// # Errors
///
/// What the reader refuses.
pub fn read_delete<'a>(reader: &mut Reader<'a>, flexible: bool) -> Result<Vec<&'a str>> {
    let count = reader.array_len(flexible, "topic_names")?.unwrap_or(0);
    let mut names = Vec::with_capacity(count);
    for _ in 0..count {
        names.push(reader.string(flexible, "topic name")?);
    }
    let _timeout = reader.i32("timeout_ms")?;
    if flexible {
        reader.tagged_fields("delete topics")?;
    }
    Ok(names)
}

/// Write a DeleteTopics response: a code per topic.
///
/// # Errors
///
/// What the writer refuses.
pub fn write_delete(writer: &mut Writer, flexible: bool, results: &[(&str, i16)]) -> Result<()> {
    writer.i32(0);
    writer.array_len(flexible, Some(results.len()))?;
    for (name, code) in results {
        writer.string(flexible, name)?;
        writer.i16(*code);
        if flexible {
            writer.no_tagged_fields();
        }
    }
    if flexible {
        writer.no_tagged_fields();
    }
    Ok(())
}
