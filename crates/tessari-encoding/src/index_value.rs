//! Encoding a value so that its **bytes** sort the way the value does.
//!
//! This is the opposite discipline to the payload codec next door. A payload is
//! read, never compared, so it is encoded for exactness. An index entry is never
//! read for its value at all — it is *scanned* — so it is encoded for order, and
//! the two encodings are deliberately different bytes for the same value.
//!
//! # This encoding is one-way, and that is the point
//!
//! Numbers are normalised: `1`, `1.0` and decimal `1.00` produce **identical**
//! bytes, because they are one value and a unique index must refuse the second
//! of them. Normalising is exactly what makes decoding impossible — the bytes no
//! longer say which of the three spellings arrived — so this module offers
//! [`put`] and [`skip`] and no `take`. An index stores ordering identity, not
//! the value; a caller that wants the value reads the record.
//!
//! [`skip`] exists because an index key can carry a record id after the value,
//! and a parser has to find where one ends and the next begins. Every encoding
//! below is therefore self-delimiting.
//!
//! # The tag table is permanent
//!
//! Tags run `0x01`…`0x0f` in the value system's rank order, so byte order across
//! types *is* the declared cross-type order. They carry the same numbers as the
//! payload codec's tags so that a hex dump reads the same way in both, but the
//! two tables are separate contracts: neither may be renumbered, and changing
//! either is a rebuild.

mod numbers;
mod shapes;
use tessari_types::Value;

use crate::error::Result;
use crate::order::{KeyReader, KeyWriter};
use crate::record_id;
pub(crate) use numbers::{put_number, skip_number};
pub(crate) use shapes::{put_geometry, skip_geometry};

/// Ends a container, and the composite field list of an index key.
///
/// Below every type tag, so a shorter sequence sorts before a longer one that
/// extends it — which is what makes `[1]` precede `[1, 2]`.
pub(crate) const END: u8 = 0x00;

const TAG_NONE: u8 = 0x01;
const TAG_NULL: u8 = 0x02;
const TAG_BOOL: u8 = 0x03;
const TAG_NUMBER: u8 = 0x04;
const TAG_STRING: u8 = 0x05;
const TAG_BYTES: u8 = 0x06;
const TAG_DURATION: u8 = 0x07;
const TAG_DATETIME: u8 = 0x08;
const TAG_UUID: u8 = 0x09;
const TAG_TABLE: u8 = 0x0a;
const TAG_RECORD: u8 = 0x0b;
const TAG_ARRAY: u8 = 0x0c;
const TAG_OBJECT: u8 = 0x0d;
const TAG_RANGE: u8 = 0x0e;
const TAG_SET: u8 = 0x0f;
const TAG_GEOMETRY: u8 = 0x10;
const TAG_REGEX: u8 = 0x11;

/// Every index value tag and the type it names, for the format surface.
pub(crate) const SURFACE: &[(u8, &str)] = &[
    (TAG_NONE, "none"),
    (TAG_NULL, "null"),
    (TAG_BOOL, "bool"),
    (TAG_NUMBER, "number"),
    (TAG_STRING, "string"),
    (TAG_BYTES, "bytes"),
    (TAG_DURATION, "duration"),
    (TAG_DATETIME, "datetime"),
    (TAG_UUID, "uuid"),
    (TAG_TABLE, "table"),
    (TAG_RECORD, "record"),
    (TAG_ARRAY, "array"),
    (TAG_OBJECT, "object"),
    (TAG_RANGE, "range"),
    (TAG_SET, "set"),
    (TAG_GEOMETRY, "geometry"),
    (TAG_REGEX, "regex"),
];

// Where a number sits before its magnitude is consulted. Ordered as bytes, so
// the declared places of the infinities and of not-a-number are simply their
// tags.
const NUMBER_NEGATIVE_INFINITY: u8 = 0x00;
const NUMBER_NEGATIVE: u8 = 0x01;
const NUMBER_ZERO: u8 = 0x02;
const NUMBER_POSITIVE: u8 = 0x03;
const NUMBER_POSITIVE_INFINITY: u8 = 0x04;
const NUMBER_NOT_A_NUMBER: u8 = 0x05;

/// Ends the digit string of a positive number: below every ASCII digit.
const DIGITS_END: u8 = 0x00;
/// Ends the digit string of a negative number: above every complemented digit.
const DIGITS_END_NEGATIVE: u8 = 0xff;

/// Ends a sequence inside an ordering key.
///
/// A sequence is written as `MORE element MORE element … END`. Because `END` is
/// below `MORE`, a shorter sequence sorts before a longer one that begins with
/// it — which is exactly how `Vec` compares, and the reason a count-prefixed
/// form would be **wrong**: `[b]` would sort before `[a, a]` on the count while
/// `Vec` puts `[a, a]` first.
const SEQUENCE_END: u8 = 0;
/// Introduces one more element of a sequence.
const SEQUENCE_MORE: u8 = 1;

const BOUND_UNBOUNDED: u8 = 0;
const BOUND_INCLUDED: u8 = 1;
const BOUND_EXCLUDED: u8 = 2;

/// The text of a lone encoded string, when that is what these bytes are.
///
/// The one direction of this encoding that **is** reversible, and the asymmetry
/// is worth stating because the type around it is documented as opaque. What
/// destroys reversibility is the number encoding: `1`, `1.0` and decimal `1.00`
/// are normalised to the same bytes, so no reader can say which was written. A
/// string is written as its own bytes under a byte-local escape and comes back
/// exactly — nothing is normalised away.
///
/// `None` for anything else, including a string followed by a second value: a
/// caller wanting the term of a search index is asking about a lone string, and
/// answering with the first of several would hand back a term nobody stored.
pub(crate) fn lone_string(bytes: &[u8]) -> Option<String> {
    let mut reader = KeyReader::new(crate::kind::KeyKind::SearchTerm, bytes);
    if reader.take_u8().ok()? != TAG_STRING {
        return None;
    }
    let text = String::from_utf8(reader.take_variable().ok()?).ok()?;
    (reader.take_u8().ok()? == END && reader.finish().is_ok()).then_some(text)
}

/// Append the bytes every encoded string beginning with `prefix` starts with.
///
/// The tag and the escaped body, and nothing else: no terminator, no end
/// marker. Bounding a scan with this asks "values beginning with `prefix`" and
/// gets exactly them, because the escape is byte-local (see
/// [`KeyWriter::put_variable_unterminated`]).
pub(crate) fn put_string_prefix(writer: &mut KeyWriter, prefix: &str) {
    writer
        .put_u8(TAG_STRING)
        .put_variable_unterminated(prefix.as_bytes());
}

/// Append `value` in its order-preserving form.
/// A float as a `u64` whose unsigned order is IEEE-754's **total** order.
///
/// Positives get their sign bit set; negatives are inverted whole. That is the
/// standard transform, and it matches [`f64::total_cmp`] — which is what
/// `Position` compares by, so the bytes here and the comparison there cannot
/// disagree. `-0.0` sorts below `0.0` in both.
const fn orderable(value: f64) -> u64 {
    let bits = value.to_bits();
    if bits & (1_u64 << 63) == 0 {
        bits ^ (1_u64 << 63)
    } else {
        !bits
    }
}

pub(crate) fn put(writer: &mut KeyWriter, value: &Value) {
    match value {
        Value::None => {
            writer.put_u8(TAG_NONE);
        }
        Value::Null => {
            writer.put_u8(TAG_NULL);
        }
        Value::Bool(flag) => {
            writer.put_u8(TAG_BOOL).put_u8(u8::from(*flag));
        }
        Value::Number(number) => {
            writer.put_u8(TAG_NUMBER);
            put_number(writer, number);
        }
        Value::String(text) => {
            writer.put_u8(TAG_STRING).put_variable(text.as_bytes());
        }
        Value::Bytes(bytes) => {
            writer.put_u8(TAG_BYTES).put_variable(bytes);
        }
        Value::Duration(duration) => {
            writer
                .put_u8(TAG_DURATION)
                .put_i64(duration.seconds())
                .put_u32(duration.nanos());
        }
        Value::Datetime(datetime) => {
            writer
                .put_u8(TAG_DATETIME)
                .put_i64(datetime.seconds())
                .put_u32(datetime.nanos());
        }
        Value::Uuid(bytes) => {
            writer.put_u8(TAG_UUID).put_fixed(bytes);
        }
        Value::Table(table) => {
            writer.put_u8(TAG_TABLE).put_u32(table.get());
        }
        Value::Record(reference) => {
            writer.put_u8(TAG_RECORD).put_u32(reference.table.get());
            record_id::put(writer, &reference.id);
        }
        Value::Array(items) => {
            writer.put_u8(TAG_ARRAY);
            for item in items {
                put(writer, item);
            }
            writer.put_u8(END);
        }
        Value::Object(fields) => {
            writer.put_u8(TAG_OBJECT);
            for (name, field) in fields {
                writer.put_variable(name.as_bytes());
                put(writer, field);
            }
            writer.put_u8(END);
        }
        Value::Range(range) => {
            writer.put_u8(TAG_RANGE);
            put_bound(writer, &range.start);
            put_bound(writer, &range.end);
        }
        Value::Set(items) => {
            writer.put_u8(TAG_SET);
            for item in items {
                put(writer, item);
            }
            writer.put_u8(END);
        }
        Value::Geometry(held) => {
            writer.put_u8(TAG_GEOMETRY);
            put_geometry(writer, held);
        }
        Value::Regex(pattern) => {
            writer.put_u8(TAG_REGEX).put_variable(pattern.as_bytes());
        }
    }
}

/// Walk past one encoded value without interpreting it.
///
/// # Errors
///
/// Returns an error when the bytes are truncated or carry an unknown tag.
/// Step over a sequence written as `MORE element … END`.
fn skip_sequence(
    reader: &mut KeyReader<'_>,
    mut element: impl FnMut(&mut KeyReader<'_>) -> Result<()>,
) -> Result<()> {
    loop {
        match reader.take_u8()? {
            SEQUENCE_END => return Ok(()),
            SEQUENCE_MORE => element(reader)?,
            other => {
                return Err(crate::error::Error::UnknownIndexTag {
                    kind: reader.kind(),
                    tag: other,
                });
            }
        }
    }
}

pub(crate) fn skip(reader: &mut KeyReader<'_>) -> Result<()> {
    let tag = reader.take_u8()?;
    match tag {
        TAG_NONE | TAG_NULL => Ok(()),
        TAG_BOOL => reader.take_u8().map(|_| ()),
        TAG_NUMBER => skip_number(reader),
        TAG_STRING | TAG_BYTES => reader.take_variable().map(|_| ()),
        TAG_DURATION | TAG_DATETIME => {
            reader.take_i64()?;
            reader.take_u32().map(|_| ())
        }
        TAG_UUID => reader.take_exact(16).map(|_| ()),
        TAG_TABLE => reader.take_u32().map(|_| ()),
        TAG_RECORD => {
            reader.take_u32()?;
            record_id::take(reader).map(|_| ())
        }
        TAG_ARRAY | TAG_SET => skip_until_end(reader, false),
        TAG_OBJECT => skip_until_end(reader, true),
        TAG_RANGE => {
            skip_bound(reader)?;
            skip_bound(reader)
        }
        TAG_GEOMETRY => skip_geometry(reader),
        TAG_REGEX => reader.take_variable().map(|_| ()),
        other => Err(crate::error::Error::UnknownIndexTag {
            kind: reader.kind(),
            tag: other,
        }),
    }
}

fn put_bound(writer: &mut KeyWriter, bound: &core::ops::Bound<Value>) {
    match bound {
        core::ops::Bound::Unbounded => {
            writer.put_u8(BOUND_UNBOUNDED);
        }
        core::ops::Bound::Included(value) => {
            writer.put_u8(BOUND_INCLUDED);
            put(writer, value);
        }
        core::ops::Bound::Excluded(value) => {
            writer.put_u8(BOUND_EXCLUDED);
            put(writer, value);
        }
    }
}

fn skip_bound(reader: &mut KeyReader<'_>) -> Result<()> {
    match reader.take_u8()? {
        BOUND_UNBOUNDED => Ok(()),
        _ => skip(reader),
    }
}

fn skip_until_end(reader: &mut KeyReader<'_>, named: bool) -> Result<()> {
    loop {
        if reader.peek()? == END {
            reader.take_u8()?;
            return Ok(());
        }
        if named {
            reader.take_variable()?;
        }
        skip(reader)?;
    }
}
