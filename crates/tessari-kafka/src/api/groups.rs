//! ListGroups, DescribeGroups and DeleteGroups: the groups as a tool sees them.

use crate::codec::{Reader, Writer};
use crate::error::Result;

/// Read a list of strings, which each of these requests opens with.
fn strings<'a>(
    reader: &mut Reader<'a>,
    flexible: bool,
    field: &'static str,
) -> Result<Vec<&'a str>> {
    let count = reader.array_len(flexible, field)?.unwrap_or(0);
    let mut names = Vec::with_capacity(count);
    for _ in 0..count {
        names.push(reader.string(flexible, field)?);
    }
    Ok(names)
}

/// Read a ListGroups request: the states asked for, empty for every state.
///
/// # Errors
///
/// What the reader refuses.
pub fn read_list<'a>(
    reader: &mut Reader<'a>,
    version: i16,
    flexible: bool,
) -> Result<Vec<&'a str>> {
    let states = if version >= 4 {
        strings(reader, flexible, "states_filter")?
    } else {
        Vec::new()
    };
    if flexible {
        reader.tagged_fields("list groups")?;
    }
    Ok(states)
}

/// One group in a listing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Listed {
    /// Its id.
    pub group: String,
    /// Its protocol type, empty for a group nobody has joined.
    pub protocol_type: String,
    /// Its state's name.
    pub state: &'static str,
}

/// Write a ListGroups response.
///
/// # Errors
///
/// What the writer refuses.
pub fn write_list(
    writer: &mut Writer,
    version: i16,
    flexible: bool,
    groups: &[Listed],
) -> Result<()> {
    if version >= 1 {
        writer.i32(0);
    }
    writer.i16(0);
    writer.array_len(flexible, Some(groups.len()))?;
    for group in groups {
        writer.string(flexible, &group.group)?;
        writer.string(flexible, &group.protocol_type)?;
        if version >= 4 {
            writer.string(flexible, group.state)?;
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

/// Read a DescribeGroups request: the groups asked about.
///
/// # Errors
///
/// What the reader refuses.
pub fn read_describe<'a>(
    reader: &mut Reader<'a>,
    version: i16,
    flexible: bool,
) -> Result<Vec<&'a str>> {
    let groups = strings(reader, flexible, "groups")?;
    if version >= 3 {
        let _operations = reader.bool("include_authorized_operations")?;
    }
    if flexible {
        reader.tagged_fields("describe groups")?;
    }
    Ok(groups)
}

/// One member as a description gives it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DescribedMember {
    /// Its id.
    pub id: String,
    /// Its instance id.
    pub instance: Option<String>,
    /// The client's name for itself.
    pub client: String,
    /// The address it joined from.
    pub host: String,
    /// Its subscription under the group's protocol.
    pub metadata: Vec<u8>,
    /// Its assignment.
    pub assignment: Vec<u8>,
}

/// One group's description.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Described {
    /// The protocol's error code.
    pub error_code: i16,
    /// Its id.
    pub group: String,
    /// Its state's name; `Dead` for a group this node does not hold.
    pub state: &'static str,
    /// Its protocol type.
    pub protocol_type: String,
    /// The protocol of its generation.
    pub protocol: String,
    /// Its members.
    pub members: Vec<DescribedMember>,
}

/// Write a DescribeGroups response.
///
/// # Errors
///
/// What the writer refuses.
pub fn write_describe(
    writer: &mut Writer,
    version: i16,
    flexible: bool,
    groups: &[Described],
) -> Result<()> {
    if version >= 1 {
        writer.i32(0);
    }
    writer.array_len(flexible, Some(groups.len()))?;
    for group in groups {
        writer.i16(group.error_code);
        writer.string(flexible, &group.group)?;
        writer.string(flexible, group.state)?;
        writer.string(flexible, &group.protocol_type)?;
        writer.string(flexible, &group.protocol)?;
        writer.array_len(flexible, Some(group.members.len()))?;
        for member in &group.members {
            writer.string(flexible, &member.id)?;
            if version >= 4 {
                writer.nullable_string(flexible, member.instance.as_deref())?;
            }
            writer.string(flexible, &member.client)?;
            writer.string(flexible, &member.host)?;
            writer.bytes(flexible, &member.metadata)?;
            writer.bytes(flexible, &member.assignment)?;
            if flexible {
                writer.no_tagged_fields();
            }
        }
        if version >= 3 {
            // Not computed: the protocol's marker for "not asked".
            writer.i32(i32::MIN);
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

/// Read a DeleteGroups request: the groups to delete.
///
/// # Errors
///
/// What the reader refuses.
pub fn read_delete<'a>(reader: &mut Reader<'a>, flexible: bool) -> Result<Vec<&'a str>> {
    let groups = strings(reader, flexible, "groups_names")?;
    if flexible {
        reader.tagged_fields("delete groups")?;
    }
    Ok(groups)
}

/// Write a DeleteGroups response: a code per group.
///
/// # Errors
///
/// What the writer refuses.
pub fn write_delete(writer: &mut Writer, flexible: bool, results: &[(&str, i16)]) -> Result<()> {
    writer.i32(0);
    writer.array_len(flexible, Some(results.len()))?;
    for (group, code) in results {
        writer.string(flexible, group)?;
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
