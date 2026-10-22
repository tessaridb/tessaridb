//! JoinGroup: a member asks into a group's next generation.

use crate::codec::{Reader, Writer};
use crate::error::Result;

/// One assignment protocol a member can speak, with its subscription.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Protocol<'a> {
    /// The protocol's name, `range` or `cooperative-sticky` for a consumer.
    pub name: &'a str,
    /// What the member subscribes to, opaque to the coordinator.
    pub metadata: &'a [u8],
}

/// A join request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Request<'a> {
    /// The group.
    pub group: &'a str,
    /// How long the member may go without a heartbeat.
    pub session_timeout_ms: i32,
    /// How long a rebalance may wait for the member to rejoin.
    pub rebalance_timeout_ms: i32,
    /// The member's id, empty on its first join.
    pub member: &'a str,
    /// A static member's instance id, echoed back.
    pub instance: Option<&'a str>,
    /// `consumer` for a consumer group.
    pub protocol_type: &'a str,
    /// The protocols the member speaks, preferred first.
    pub protocols: Vec<Protocol<'a>>,
}

impl<'a> Request<'a> {
    /// Read the body at `version`.
    ///
    /// # Errors
    ///
    /// What the reader refuses.
    pub fn read(reader: &mut Reader<'a>, version: i16, flexible: bool) -> Result<Self> {
        let group = reader.string(flexible, "group_id")?;
        let session_timeout_ms = reader.i32("session_timeout_ms")?;
        // Before version 1 the session timeout also bounds the rebalance.
        let rebalance_timeout_ms = if version >= 1 {
            reader.i32("rebalance_timeout_ms")?
        } else {
            session_timeout_ms
        };
        let member = reader.string(flexible, "member_id")?;
        let instance = if version >= 5 {
            reader.nullable_string(flexible, "group_instance_id")?
        } else {
            None
        };
        let protocol_type = reader.string(flexible, "protocol_type")?;
        let count = reader.array_len(flexible, "protocols")?.unwrap_or(0);
        let mut protocols = Vec::with_capacity(count);
        for _ in 0..count {
            let name = reader.string(flexible, "protocol name")?;
            let metadata = reader.bytes(flexible, "protocol metadata")?;
            if flexible {
                reader.tagged_fields("join group protocol")?;
            }
            protocols.push(Protocol { name, metadata });
        }
        if flexible {
            reader.tagged_fields("join group")?;
        }
        Ok(Self {
            group,
            session_timeout_ms,
            rebalance_timeout_ms,
            member,
            instance,
            protocol_type,
            protocols,
        })
    }
}

/// One member as the leader is told of it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Member {
    /// Its id.
    pub id: String,
    /// Its instance id, when it gave one.
    pub instance: Option<String>,
    /// Its subscription under the chosen protocol.
    pub metadata: Vec<u8>,
}

/// A join's answer.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Answer {
    /// The protocol's error code.
    pub error_code: i16,
    /// The generation joined, −1 on an error.
    pub generation: i32,
    /// The protocol type.
    pub protocol_type: Option<String>,
    /// The protocol chosen for the generation.
    pub protocol: Option<String>,
    /// The leader's member id.
    pub leader: String,
    /// The member's own id.
    pub member: String,
    /// Every member, for the leader only.
    pub members: Vec<Member>,
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
    answer: &Answer,
) -> Result<()> {
    if version >= 2 {
        writer.i32(0);
    }
    writer.i16(answer.error_code);
    writer.i32(answer.generation);
    if version >= 7 {
        writer.nullable_string(flexible, answer.protocol_type.as_deref())?;
        writer.nullable_string(flexible, answer.protocol.as_deref())?;
    } else {
        writer.string(flexible, answer.protocol.as_deref().unwrap_or(""))?;
    }
    writer.string(flexible, &answer.leader)?;
    writer.string(flexible, &answer.member)?;
    writer.array_len(flexible, Some(answer.members.len()))?;
    for member in &answer.members {
        writer.string(flexible, &member.id)?;
        if version >= 5 {
            writer.nullable_string(flexible, member.instance.as_deref())?;
        }
        writer.bytes(flexible, &member.metadata)?;
        if flexible {
            writer.no_tagged_fields();
        }
    }
    if flexible {
        writer.no_tagged_fields();
    }
    Ok(())
}
