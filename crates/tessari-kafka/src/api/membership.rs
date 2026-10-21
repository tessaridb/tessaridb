//! SyncGroup, Heartbeat and LeaveGroup: a member inside a generation.

use crate::codec::{Reader, Writer};
use crate::error::Result;

/// A member's place in a generation, which every one of these names.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Place<'a> {
    /// The group.
    pub group: &'a str,
    /// The generation the member believes it is in.
    pub generation: i32,
    /// The member.
    pub member: &'a str,
    /// A static member's instance id.
    pub instance: Option<&'a str>,
}

impl<'a> Place<'a> {
    fn read(
        reader: &mut Reader<'a>,
        flexible: bool,
        instance_from: i16,
        version: i16,
    ) -> Result<Self> {
        let group = reader.string(flexible, "group_id")?;
        let generation = reader.i32("generation_id")?;
        let member = reader.string(flexible, "member_id")?;
        let instance = if version >= instance_from {
            reader.nullable_string(flexible, "group_instance_id")?
        } else {
            None
        };
        Ok(Self {
            group,
            generation,
            member,
            instance,
        })
    }
}

/// A sync request: the leader's assignments, or nothing from anyone else.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Sync<'a> {
    /// Who asks.
    pub place: Place<'a>,
    /// Each member's assignment, sent by the leader.
    pub assignments: Vec<(&'a str, &'a [u8])>,
}

impl<'a> Sync<'a> {
    /// Read the body at `version`.
    ///
    /// # Errors
    ///
    /// What the reader refuses.
    pub fn read(reader: &mut Reader<'a>, version: i16, flexible: bool) -> Result<Self> {
        let place = Place::read(reader, flexible, 3, version)?;
        if version >= 5 {
            let _type = reader.nullable_string(flexible, "protocol_type")?;
            let _name = reader.nullable_string(flexible, "protocol_name")?;
        }
        let count = reader.array_len(flexible, "assignments")?.unwrap_or(0);
        let mut assignments = Vec::with_capacity(count);
        for _ in 0..count {
            let member = reader.string(flexible, "assignment member")?;
            let assignment = reader.bytes(flexible, "assignment")?;
            if flexible {
                reader.tagged_fields("sync group assignment")?;
            }
            assignments.push((member, assignment));
        }
        if flexible {
            reader.tagged_fields("sync group")?;
        }
        Ok(Self { place, assignments })
    }
}

/// A sync's answer.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Synced {
    /// The protocol's error code.
    pub error_code: i16,
    /// The protocol type, from version 5.
    pub protocol_type: Option<String>,
    /// The protocol, from version 5.
    pub protocol: Option<String>,
    /// The member's assignment.
    pub assignment: Vec<u8>,
}

/// Write a sync's response at `version`.
///
/// # Errors
///
/// What the writer refuses.
pub fn write_sync(
    writer: &mut Writer,
    version: i16,
    flexible: bool,
    synced: &Synced,
) -> Result<()> {
    if version >= 1 {
        writer.i32(0);
    }
    writer.i16(synced.error_code);
    if version >= 5 {
        writer.nullable_string(flexible, synced.protocol_type.as_deref())?;
        writer.nullable_string(flexible, synced.protocol.as_deref())?;
    }
    writer.bytes(flexible, &synced.assignment)?;
    if flexible {
        writer.no_tagged_fields();
    }
    Ok(())
}

/// Read a heartbeat at `version`.
///
/// # Errors
///
/// What the reader refuses.
pub fn read_heartbeat<'a>(
    reader: &mut Reader<'a>,
    version: i16,
    flexible: bool,
) -> Result<Place<'a>> {
    let place = Place::read(reader, flexible, 3, version)?;
    if flexible {
        reader.tagged_fields("heartbeat")?;
    }
    Ok(place)
}

/// Write a heartbeat's response.
pub fn write_heartbeat(writer: &mut Writer, version: i16, flexible: bool, error_code: i16) {
    if version >= 1 {
        writer.i32(0);
    }
    writer.i16(error_code);
    if flexible {
        writer.no_tagged_fields();
    }
}

/// A leave request: one member before version 3, a list from it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Leave<'a> {
    /// The group.
    pub group: &'a str,
    /// The members leaving, each with its instance id.
    pub members: Vec<(&'a str, Option<&'a str>)>,
}

impl<'a> Leave<'a> {
    /// Read the body at `version`.
    ///
    /// # Errors
    ///
    /// What the reader refuses.
    pub fn read(reader: &mut Reader<'a>, version: i16, flexible: bool) -> Result<Self> {
        let group = reader.string(flexible, "group_id")?;
        let mut members = Vec::new();
        if version < 3 {
            members.push((reader.string(flexible, "member_id")?, None));
        } else {
            let count = reader.array_len(flexible, "members")?.unwrap_or(0);
            members.reserve(count);
            for _ in 0..count {
                let member = reader.string(flexible, "leaving member")?;
                let instance = reader.nullable_string(flexible, "leaving instance")?;
                if flexible {
                    reader.tagged_fields("leave group member")?;
                }
                members.push((member, instance));
            }
        }
        if flexible {
            reader.tagged_fields("leave group")?;
        }
        Ok(Self { group, members })
    }
}

/// Write a leave's response: one code, and from version 3 one per member.
///
/// # Errors
///
/// What the writer refuses.
pub fn write_leave(
    writer: &mut Writer,
    version: i16,
    flexible: bool,
    error_code: i16,
    members: &[(&str, Option<&str>, i16)],
) -> Result<()> {
    if version >= 1 {
        writer.i32(0);
    }
    writer.i16(error_code);
    if version >= 3 {
        writer.array_len(flexible, Some(members.len()))?;
        for (member, instance, code) in members {
            writer.string(flexible, member)?;
            writer.nullable_string(flexible, *instance)?;
            writer.i16(*code);
            if flexible {
                writer.no_tagged_fields();
            }
        }
    }
    if flexible {
        writer.no_tagged_fields();
    }
    Ok(())
}
