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

use std::collections::{BTreeMap, BTreeSet};
use std::ops::Bound;

use bgv_db_kv::Value as StoredBytes;
use bgv_db_types::{Datetime, Duration, Number, RecordId, RecordRef, TableId, Value, ValueRange};
use rust_decimal::Decimal;

use crate::error::{Error, Result};
use crate::kind::KeyKind;
use crate::order::{KeyReader, KeyWriter};
use crate::record_id;

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
    let value = take_value(&mut reader)?;
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
    }
}

fn take_value(reader: &mut KeyReader<'_>) -> Result<Value> {
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
            let count = reader.take_u32()?;
            let mut items = Vec::new();
            for _ in 0..count {
                items.push(take_value(reader)?);
            }
            Ok(Value::Array(items))
        }
        tag::OBJECT => {
            let count = reader.take_u32()?;
            let mut fields = BTreeMap::new();
            for _ in 0..count {
                let name = take_bytes(reader)?;
                let name = String::from_utf8(name).map_err(|_| Error::InvalidUtf8 {
                    kind: KeyKind::Record,
                })?;
                fields.insert(name, take_value(reader)?);
            }
            Ok(Value::Object(fields))
        }
        tag::RANGE => {
            let start = take_bound(reader)?;
            let end = take_bound(reader)?;
            Ok(Value::Range(Box::new(ValueRange::new(start, end))))
        }
        tag::SET => {
            let count = reader.take_u32()?;
            let mut items = BTreeSet::new();
            for _ in 0..count {
                items.insert(take_value(reader)?);
            }
            Ok(Value::Set(items))
        }
        unknown => Err(Error::UnknownValueTag { tag: unknown }),
    }
}

fn put_number(writer: &mut KeyWriter, number: &Number) {
    match number {
        Number::Integer(value) => {
            writer.put_u8(number_kind::INTEGER).put_i64(*value);
        }
        Number::Float(value) => {
            writer
                .put_u8(number_kind::FLOAT)
                .put_fixed(&value.to_bits().to_be_bytes());
        }
        Number::Decimal(value) => {
            writer
                .put_u8(number_kind::DECIMAL)
                .put_fixed(&value.mantissa().to_be_bytes())
                .put_u32(value.scale());
        }
    }
}

fn take_number(reader: &mut KeyReader<'_>) -> Result<Number> {
    match reader.take_u8()? {
        number_kind::INTEGER => Ok(Number::Integer(reader.take_i64()?)),
        number_kind::FLOAT => {
            let bits = u64::from_be_bytes(reader.take_fixed::<8>()?);
            Ok(Number::float(f64::from_bits(bits)))
        }
        number_kind::DECIMAL => {
            let mantissa = i128::from_be_bytes(reader.take_fixed::<16>()?);
            let scale = reader.take_u32()?;
            Decimal::try_from_i128_with_scale(mantissa, scale)
                .map(Number::Decimal)
                .map_err(|_| Error::InvalidDecimal { mantissa, scale })
        }
        unknown => Err(Error::UnknownValueTag { tag: unknown }),
    }
}

fn put_bound(writer: &mut KeyWriter, bound: &Bound<Value>) {
    match bound {
        Bound::Unbounded => {
            writer.put_u8(bound_kind::UNBOUNDED);
        }
        Bound::Included(value) => {
            writer.put_u8(bound_kind::INCLUDED);
            put_value(writer, value);
        }
        Bound::Excluded(value) => {
            writer.put_u8(bound_kind::EXCLUDED);
            put_value(writer, value);
        }
    }
}

fn take_bound(reader: &mut KeyReader<'_>) -> Result<Bound<Value>> {
    match reader.take_u8()? {
        bound_kind::UNBOUNDED => Ok(Bound::Unbounded),
        bound_kind::INCLUDED => Ok(Bound::Included(take_value(reader)?)),
        bound_kind::EXCLUDED => Ok(Bound::Excluded(take_value(reader)?)),
        unknown => Err(Error::UnknownValueTag { tag: unknown }),
    }
}

fn take_time(reader: &mut KeyReader<'_>) -> Result<(i64, u32)> {
    Ok((reader.take_i64()?, reader.take_u32()?))
}

fn put_bytes(writer: &mut KeyWriter, bytes: &[u8]) {
    writer.put_u32(count_of(bytes.len())).put_fixed(bytes);
}

fn take_bytes(reader: &mut KeyReader<'_>) -> Result<Vec<u8>> {
    let len = reader.take_u32()?;
    reader.take_exact(usize::try_from(len).unwrap_or(usize::MAX))
}

/// A length or item count as it is written.
///
/// A collection with more members than a `u32` counts is not something this
/// store can hold — it would have exhausted memory long before the codec sees
/// it — and saturating keeps the encoder total instead of making every caller
/// handle a case that cannot arise. The decoder rejects the result as truncated,
/// so the failure is loud either way.
fn count_of(len: usize) -> u32 {
    u32::try_from(len).unwrap_or(u32::MAX)
}
