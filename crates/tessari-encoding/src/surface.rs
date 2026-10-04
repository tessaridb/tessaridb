//! The format surface: every byte value the on-disk format gives a meaning to.
//!
//! Each owning module lists its own values beside the constants it decodes
//! with; this module only gathers them. What the list is for is to hold the
//! written format (`docs/key-grammar.md`, `docs/value-system.md`) and the code
//! to one another — a test reads both and refuses a value one of them has and
//! the other does not — and to let a change to any of them be told apart from
//! a change to none, which is what decides whether the format version moves.

use crate::kind::KeyKind;
use crate::value::{CODEC_VERSION, FLAGS, FormatVersion};
use crate::{index_value, payload, record_id};

/// Which table of the format a value belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Family {
    /// The leading byte of a key, naming its kind.
    KeyKind,
    /// The keyspace a key kind is stored in, keyed by the kind's tag.
    Keyspace,
    /// The discriminant in front of a record id.
    RecordId,
    /// The leading byte of an index value, in cross-type order.
    IndexValue,
    /// The leading byte of a stored payload.
    Payload,
    /// The byte naming a number's kind inside a payload.
    NumberKind,
    /// The byte naming a geometry's shape inside a payload.
    Shape,
    /// The byte naming a range bound's kind inside a payload.
    BoundKind,
    /// A bit of the flags byte every stored value carries.
    ValueFlag,
    /// The codec version every stored value begins with.
    Codec,
    /// The store's own format version.
    Format,
}

/// One value of the format and the name it carries.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct FormatUnit {
    /// The table it belongs to.
    pub family: Family,
    /// The value on disk: a byte, a flag mask, or a version number.
    pub code: u32,
    /// What it is called in the code and in the written format.
    pub name: &'static str,
}

/// Every value the format gives a meaning to, in family then code order.
#[must_use]
pub fn units() -> Vec<FormatUnit> {
    let bytes = |family: Family, table: &'static [(u8, &'static str)]| {
        table.iter().map(move |&(code, name)| FormatUnit {
            family,
            code: u32::from(code),
            name,
        })
    };
    let mut units: Vec<FormatUnit> = KeyKind::ALL
        .iter()
        .flat_map(|&kind| {
            [
                FormatUnit {
                    family: Family::KeyKind,
                    code: u32::from(kind.tag()),
                    name: kind.name(),
                },
                FormatUnit {
                    family: Family::Keyspace,
                    code: u32::from(kind.tag()),
                    name: kind.keyspace().name(),
                },
            ]
        })
        .chain(bytes(Family::RecordId, record_id::SURFACE))
        .chain(bytes(Family::IndexValue, index_value::SURFACE))
        .chain(bytes(Family::Payload, payload::TAGS))
        .chain(bytes(Family::NumberKind, payload::NUMBER_KINDS))
        .chain(bytes(Family::Shape, payload::SHAPES))
        .chain(bytes(Family::BoundKind, payload::BOUND_KINDS))
        .chain(bytes(Family::ValueFlag, FLAGS))
        .collect();
    units.push(FormatUnit {
        family: Family::Codec,
        code: u32::from(CODEC_VERSION),
        name: "codec",
    });
    units.push(FormatUnit {
        family: Family::Format,
        code: FormatVersion::CURRENT.get(),
        name: "format",
    });
    units.sort_unstable();
    units
}
