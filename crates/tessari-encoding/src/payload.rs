//! The codec for what a record's payload holds.
//!
//! A record version already carries a versioned envelope with a tombstone flag.
//! This is what goes *inside* it: the typed value itself. Keeping the two apart
//! means the store can tell "this record was deleted" from "this record holds
//! nothing" without either question reaching into the other's bytes.
//!
//! # Every value starts with its type
//!
//! The first byte names the type, and an unknown one is refused rather than
//! guessed at. A codec that infers the type from what follows reads a newer
//! format as a plausible wrong value, and nothing downstream can tell.
//!
//! The tags below are **permanent**. A tag is never reused for a different type
//! and never renumbered, for the same reason key kinds are not: data already
//! written carries them.
//!
//! | Tag | Type |
//! |---|---|
//! | `0x01` | none |
//! | `0x02` | null |
//! | `0x03` | bool |
//! | `0x04` | number |
//! | `0x05` | string |
//! | `0x06` | bytes |
//! | `0x07` | duration |
//! | `0x08` | datetime |
//! | `0x09` | uuid |
//! | `0x0a` | table |
//! | `0x0b` | record |
//! | `0x0c` | array |
//! | `0x0d` | object |
//! | `0x0e` | range |
//! | `0x0f` | set |
//! | `0x10` | geometry |
//! | `0x11` | regex |
//!
//! # A decimal is stored in our terms, not the library's
//!
//! An exact decimal is written as its unscaled value and its number of
//! fractional digits — the two numbers that define it — rather than as whatever
//! the arithmetic library keeps in memory. Writing the library's own layout to
//! disk would make a dependency upgrade a data migration, and would do it
//! silently, since the bytes would still parse.
//!
//! # This encoding is not order-preserving, and does not need to be
//!
//! Keys are what sort; a payload is read, not compared byte by byte. Values are
//! ordered by [`Value`]'s own comparison, which is semantic — three spellings of
//! the number one compare equal there and could not possibly encode to the same
//! bytes here. Ordering the payload bytes as well would be a second ordering
//! authority disagreeing with the first.

mod scalars;
mod shapes;
use std::collections::{BTreeMap, BTreeSet};

use tessari_kv::Value as StoredBytes;
use tessari_types::{
    Datetime, Duration, MAX_NESTING, RecordId, RecordRef, TableId, Value, ValueRange,
};

use crate::error::{Error, Result};
use crate::kind::KeyKind;
use crate::order::{KeyReader, KeyWriter};
use crate::record_id;
pub(crate) use scalars::{
    count_of, put_bound, put_bytes, put_number, take_bound, take_bytes, take_number, take_time,
};
pub(crate) use shapes::{put_geometry, take_geometry};

mod tag {
    pub(super) const NONE: u8 = 0x01;
    pub(super) const NULL: u8 = 0x02;
    pub(super) const BOOL: u8 = 0x03;
    pub(super) const NUMBER: u8 = 0x04;
    pub(super) const STRING: u8 = 0x05;
    pub(super) const BYTES: u8 = 0x06;
    pub(super) const DURATION: u8 = 0x07;
    pub(super) const DATETIME: u8 = 0x08;
    pub(super) const UUID: u8 = 0x09;
    pub(super) const TABLE: u8 = 0x0a;
    pub(super) const RECORD: u8 = 0x0b;
    pub(super) const ARRAY: u8 = 0x0c;
    pub(super) const OBJECT: u8 = 0x0d;
    pub(super) const RANGE: u8 = 0x0e;
    pub(super) const SET: u8 = 0x0f;
    pub(super) const GEOMETRY: u8 = 0x10;
    pub(super) const REGEX: u8 = 0x11;
}

/// Which shape a geometry is.
///
/// Numbered and permanent, for the reason the value tags are: a shape already
/// written carries this byte.
mod shape {
    pub(super) const POINT: u8 = 0x01;
    pub(super) const LINE: u8 = 0x02;
    pub(super) const POLYGON: u8 = 0x03;
    pub(super) const MULTI_POINT: u8 = 0x04;
    pub(super) const MULTI_LINE: u8 = 0x05;
    pub(super) const MULTI_POLYGON: u8 = 0x06;
    pub(super) const COLLECTION: u8 = 0x07;
}

mod number_kind {
    pub(super) const INTEGER: u8 = 0x01;
    pub(super) const FLOAT: u8 = 0x02;
    pub(super) const DECIMAL: u8 = 0x03;
}

mod bound_kind {
    pub(super) const UNBOUNDED: u8 = 0x01;
    pub(super) const INCLUDED: u8 = 0x02;
    pub(super) const EXCLUDED: u8 = 0x03;
}

/// Every payload type tag and the type it names, for the format surface.
pub(crate) const TAGS: &[(u8, &str)] = &[
    (tag::NONE, "none"),
    (tag::NULL, "null"),
    (tag::BOOL, "bool"),
    (tag::NUMBER, "number"),
    (tag::STRING, "string"),
    (tag::BYTES, "bytes"),
    (tag::DURATION, "duration"),
    (tag::DATETIME, "datetime"),
    (tag::UUID, "uuid"),
    (tag::TABLE, "table"),
    (tag::RECORD, "record"),
    (tag::ARRAY, "array"),
    (tag::OBJECT, "object"),
    (tag::RANGE, "range"),
    (tag::SET, "set"),
    (tag::GEOMETRY, "geometry"),
    (tag::REGEX, "regex"),
];

/// Every shape byte of a geometry, for the format surface.
pub(crate) const SHAPES: &[(u8, &str)] = &[
    (shape::POINT, "Point"),
    (shape::LINE, "LineString"),
    (shape::POLYGON, "Polygon"),
    (shape::MULTI_POINT, "MultiPoint"),
    (shape::MULTI_LINE, "MultiLineString"),
    (shape::MULTI_POLYGON, "MultiPolygon"),
    (shape::COLLECTION, "GeometryCollection"),
];

/// Every kind byte of a number, for the format surface.
pub(crate) const NUMBER_KINDS: &[(u8, &str)] = &[
    (number_kind::INTEGER, "integer"),
    (number_kind::FLOAT, "float"),
    (number_kind::DECIMAL, "decimal"),
];

/// Every kind byte of a range bound, for the format surface.
pub(crate) const BOUND_KINDS: &[(u8, &str)] = &[
    (bound_kind::UNBOUNDED, "unbounded"),
    (bound_kind::INCLUDED, "included"),
    (bound_kind::EXCLUDED, "excluded"),
];

/// Encode a value into the bytes a record payload carries.
#[must_use]
pub fn encode(value: &Value) -> StoredBytes {
    let mut writer = KeyWriter::with_capacity(16);
    put_value(&mut writer, value);
    StoredBytes::from(writer.finish())
}

/// Decode a record payload back into a value.
///
/// # Errors
///
/// Returns an error when the bytes are truncated, carry an unknown type tag,
/// hold text that is not valid UTF-8, or describe a number or a time that is not
/// representable.
pub fn decode(bytes: &[u8]) -> Result<Value> {
    // The reader is the crate's bounds-checked cursor. The kind names the entity
    // in a truncation message; no kind tag is consumed, because this is a value
    // payload rather than a key.
    let mut reader = KeyReader::new(KeyKind::Record, bytes);
    let value = take_value(&mut reader, 0)?;
    reader.finish()?;
    Ok(value)
}

fn put_value(writer: &mut KeyWriter, value: &Value) {
    match value {
        Value::None => {
            writer.put_u8(tag::NONE);
        }
        Value::Null => {
            writer.put_u8(tag::NULL);
        }
        Value::Bool(flag) => {
            writer.put_u8(tag::BOOL).put_u8(u8::from(*flag));
        }
        Value::Number(number) => {
            writer.put_u8(tag::NUMBER);
            put_number(writer, number);
        }
        Value::String(text) => {
            writer.put_u8(tag::STRING);
            put_bytes(writer, text.as_bytes());
        }
        Value::Bytes(bytes) => {
            writer.put_u8(tag::BYTES);
            put_bytes(writer, bytes);
        }
        Value::Duration(span) => {
            writer
                .put_u8(tag::DURATION)
                .put_i64(span.seconds())
                .put_u32(span.nanos());
        }
        Value::Datetime(instant) => {
            writer
                .put_u8(tag::DATETIME)
                .put_i64(instant.seconds())
                .put_u32(instant.nanos());
        }
        Value::Uuid(bytes) => {
            writer.put_u8(tag::UUID).put_fixed(bytes);
        }
        Value::Table(table) => {
            writer.put_u8(tag::TABLE).put_u32(table.get());
        }
        Value::Record(reference) => {
            writer.put_u8(tag::RECORD).put_u32(reference.table.get());
            record_id::put(writer, &reference.id);
        }
        Value::Array(items) => {
            writer.put_u8(tag::ARRAY).put_u32(count_of(items.len()));
            for item in items {
                put_value(writer, item);
            }
        }
        Value::Object(fields) => {
            writer.put_u8(tag::OBJECT).put_u32(count_of(fields.len()));
            for (name, field) in fields {
                put_bytes(writer, name.as_bytes());
                put_value(writer, field);
            }
        }
        Value::Range(range) => {
            writer.put_u8(tag::RANGE);
            put_bound(writer, &range.start);
            put_bound(writer, &range.end);
        }
        Value::Set(items) => {
            writer.put_u8(tag::SET).put_u32(count_of(items.len()));
            for item in items {
                put_value(writer, item);
            }
        }
        Value::Geometry(held) => {
            writer.put_u8(tag::GEOMETRY);
            put_geometry(writer, held);
        }
        Value::Regex(pattern) => {
            writer.put_u8(tag::REGEX);
            put_bytes(writer, pattern.as_bytes());
        }
    }
}

/// One value, inside `depth` containers.
///
/// The depth travels down so a payload nested past [`MAX_NESTING`] is refused
/// before it is followed: every level is a stack frame, and bytes off the wire
/// are read here before anybody has signed in.
fn take_value(reader: &mut KeyReader<'_>, depth: usize) -> Result<Value> {
    let tag = reader.take_u8()?;
    match tag {
        tag::NONE => Ok(Value::None),
        tag::NULL => Ok(Value::Null),
        tag::BOOL => Ok(Value::Bool(reader.take_u8()? != 0)),
        tag::NUMBER => Ok(Value::Number(take_number(reader)?)),
        tag::STRING => {
            let bytes = take_bytes(reader)?;
            let text = String::from_utf8(bytes).map_err(|_| Error::InvalidUtf8 {
                kind: KeyKind::Record,
            })?;
            Ok(Value::String(text))
        }
        tag::BYTES => Ok(Value::Bytes(take_bytes(reader)?)),
        tag::DURATION => {
            let (seconds, nanos) = take_time(reader)?;
            Duration::new(seconds, nanos)
                .map(Value::Duration)
                .ok_or(Error::InvalidSubSecond { nanos })
        }
        tag::DATETIME => {
            let (seconds, nanos) = take_time(reader)?;
            Datetime::new(seconds, nanos)
                .map(Value::Datetime)
                .ok_or(Error::InvalidSubSecond { nanos })
        }
        tag::UUID => Ok(Value::Uuid(reader.take_fixed::<16>()?)),
        tag::TABLE => Ok(Value::Table(TableId::new(reader.take_u32()?))),
        tag::RECORD => {
            let table = TableId::new(reader.take_u32()?);
            let id: RecordId = record_id::take(reader)?;
            Ok(Value::Record(RecordRef::new(table, id)))
        }
        tag::ARRAY => {
            let inside = deeper(depth)?;
            let count = reader.take_u32()?;
            let mut items = Vec::new();
            for _ in 0..count {
                items.push(take_value(reader, inside)?);
            }
            Ok(Value::Array(items))
        }
        tag::OBJECT => {
            let inside = deeper(depth)?;
            let count = reader.take_u32()?;
            let mut fields = BTreeMap::new();
            for _ in 0..count {
                let name = take_bytes(reader)?;
                let name = String::from_utf8(name).map_err(|_| Error::InvalidUtf8 {
                    kind: KeyKind::Record,
                })?;
                fields.insert(name, take_value(reader, inside)?);
            }
            Ok(Value::Object(fields))
        }
        tag::RANGE => {
            let inside = deeper(depth)?;
            let start = take_bound(reader, inside)?;
            let end = take_bound(reader, inside)?;
            Ok(Value::Range(Box::new(ValueRange::new(start, end))))
        }
        tag::SET => {
            let inside = deeper(depth)?;
            let count = reader.take_u32()?;
            let mut items = BTreeSet::new();
            for _ in 0..count {
                items.insert(take_value(reader, inside)?);
            }
            Ok(Value::Set(items))
        }
        tag::GEOMETRY => Ok(Value::Geometry(take_geometry(reader, depth)?)),
        tag::REGEX => {
            let bytes = take_bytes(reader)?;
            String::from_utf8(bytes)
                .map(Value::Regex)
                .map_err(|_| Error::InvalidUtf8 {
                    kind: KeyKind::Record,
                })
        }
        unknown => Err(Error::UnknownValueTag { tag: unknown }),
    }
}

/// The depth inside the container being entered, or the refusal when that is
/// one level more than [`MAX_NESTING`] — counted the way
/// [`Value::nests_deeper_than`] counts, so the decoder and a write agree on
/// exactly which values exist.
fn deeper(depth: usize) -> Result<usize> {
    let inside = depth.saturating_add(1);
    if inside > MAX_NESTING {
        return Err(Error::NestedTooDeep { limit: MAX_NESTING });
    }
    Ok(inside)
}
