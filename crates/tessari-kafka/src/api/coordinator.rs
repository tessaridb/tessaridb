//! FindCoordinator: which broker coordinates a group — this node, always.

use crate::codec::{Reader, Writer};
use crate::error::Result;

/// The key type of a consumer group.
pub const GROUP: i8 = 0;

/// What was asked: the key type and the keys (one before version 4).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Request<'a> {
    /// `0` a group, `1` a transactional id.
    pub key_type: i8,
    /// The keys asked about.
    pub keys: Vec<&'a str>,
}

impl<'a> Request<'a> {
    /// Read the body at `version`.
    ///
    /// # Errors
    ///
    /// What the reader refuses.
    pub fn read(reader: &mut Reader<'a>, version: i16, flexible: bool) -> Result<Self> {
        let mut keys = Vec::new();
        if version < 4 {
            keys.push(reader.string(flexible, "key")?);
        }
        let key_type = if version >= 1 {
            reader.i8("key_type")?
        } else {
            GROUP
        };
        if version >= 4 {
            let count = reader.array_len(flexible, "coordinator_keys")?.unwrap_or(0);
            keys.reserve(count);
            for _ in 0..count {
                keys.push(reader.string(flexible, "coordinator key")?);
            }
        }
        if flexible {
            reader.tagged_fields("find coordinator")?;
        }
        Ok(Self { key_type, keys })
    }
}

/// The coordinator named for one key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Answer<'a> {
    /// The key it answers.
    pub key: &'a str,
    /// The protocol's error code.
    pub error_code: i16,
    /// The broker's id.
    pub node_id: i32,
    /// Its host.
    pub host: &'a str,
    /// Its port.
    pub port: i32,
}

/// Write the response at `version`: one answer before version 4, a list from it.
///
/// # Errors
///
/// What the writer refuses.
pub fn write_response(
    writer: &mut Writer,
    version: i16,
    flexible: bool,
    answers: &[Answer<'_>],
) -> Result<()> {
    if version >= 1 {
        writer.i32(0);
    }
    if version >= 4 {
        writer.array_len(flexible, Some(answers.len()))?;
        for answer in answers {
            writer.string(flexible, answer.key)?;
            writer.i32(answer.node_id);
            writer.string(flexible, answer.host)?;
            writer.i32(answer.port);
            writer.i16(answer.error_code);
            writer.nullable_string(flexible, None)?;
            if flexible {
                writer.no_tagged_fields();
            }
        }
    } else {
        let first = answers.first();
        writer.i16(first.map_or(0, |answer| answer.error_code));
        if version >= 1 {
            writer.nullable_string(flexible, None)?;
        }
        writer.i32(first.map_or(-1, |answer| answer.node_id));
        writer.string(flexible, first.map_or("", |answer| answer.host))?;
        writer.i32(first.map_or(-1, |answer| answer.port));
    }
    if flexible {
        writer.no_tagged_fields();
    }
    Ok(())
}
