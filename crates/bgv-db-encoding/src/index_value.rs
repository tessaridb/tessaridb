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

use bgv_db_types::{Number, Value};
use rust_decimal::Decimal;

use crate::error::Result;
use crate::order::{KeyReader, KeyWriter};
use crate::record_id;

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

const BOUND_UNBOUNDED: u8 = 0;
const BOUND_INCLUDED: u8 = 1;
const BOUND_EXCLUDED: u8 = 2;

/// Append `value` in its order-preserving form.
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
    }
}

/// Walk past one encoded value without interpreting it.
///
/// # Errors
///
/// Returns an error when the bytes are truncated or carry an unknown tag.
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

/// Encode a number in the form its comparison reduces to.
///
/// The digits come from [`Number::as_decimal`] whenever a decimal can hold the
/// number, because that is the form the ordering itself uses. Taking a float's
/// own digits instead would agree everywhere obvious and disagree exactly where
/// it matters: a float too small for a decimal compares **equal to zero**, and
/// its own digits would place it just above.
fn put_number(writer: &mut KeyWriter, number: &Number) {
    if let Number::Float(value) = number {
        if value.is_nan() {
            writer.put_u8(NUMBER_NOT_A_NUMBER);
            return;
        }
        if *value == f64::INFINITY {
            writer.put_u8(NUMBER_POSITIVE_INFINITY);
            return;
        }
        if *value == f64::NEG_INFINITY {
            writer.put_u8(NUMBER_NEGATIVE_INFINITY);
            return;
        }
    }

    let form = match number.as_decimal() {
        Some(decimal) => decimal_form(decimal),
        // A finite float beyond decimal range. Its magnitude exceeds anything a
        // decimal holds, so encoding its own digits keeps it above every decimal
        // of the same sign — which is the order the comparison declares.
        None => match number {
            Number::Float(value) => float_form(*value),
            _ => None,
        },
    };

    let Some(Magnitude {
        negative,
        exponent,
        digits,
    }) = form
    else {
        writer.put_u8(NUMBER_ZERO);
        return;
    };

    let mut exponent_bytes = exponent.to_be_bytes();
    // Sign-flip so negative exponents sort below positive ones.
    exponent_bytes[0] ^= 0x80;

    if negative {
        // Every byte is complemented, so a larger magnitude sorts lower — which
        // is what "more negative" means.
        writer.put_u8(NUMBER_NEGATIVE);
        for byte in &mut exponent_bytes {
            *byte = !*byte;
        }
        writer.put_fixed(&exponent_bytes);
        for digit in &digits {
            writer.put_u8(!*digit);
        }
        writer.put_u8(DIGITS_END_NEGATIVE);
    } else {
        writer.put_u8(NUMBER_POSITIVE);
        writer.put_fixed(&exponent_bytes);
        writer.put_fixed(&digits);
        writer.put_u8(DIGITS_END);
    }
}

fn skip_number(reader: &mut KeyReader<'_>) -> Result<()> {
    let class = reader.take_u8()?;
    let terminator = match class {
        NUMBER_NEGATIVE => DIGITS_END_NEGATIVE,
        NUMBER_POSITIVE => DIGITS_END,
        _ => return Ok(()),
    };
    reader.take_exact(4)?;
    loop {
        if reader.take_u8()? == terminator {
            return Ok(());
        }
    }
}

/// A finite non-zero number as sign, decimal exponent and significant digits.
///
/// The value is `0.<digits> × 10^exponent`, which is the form in which
/// comparing the exponent and then the digit string is the same as comparing the
/// numbers.
struct Magnitude {
    negative: bool,
    exponent: i32,
    digits: Vec<u8>,
}

fn decimal_form(decimal: Decimal) -> Option<Magnitude> {
    let mantissa = decimal.mantissa();
    if mantissa == 0 {
        return None;
    }
    let digits = mantissa.unsigned_abs().to_string().into_bytes();
    let exponent = digit_count(digits.len()).saturating_sub(digit_count_of(decimal.scale()));
    Some(Magnitude {
        negative: mantissa < 0,
        exponent,
        digits: strip_trailing_zeros(digits),
    })
}

/// The digits of a float that no decimal can hold.
///
/// `{:e}` is the shortest form that round-trips, which is exactly the digit
/// string wanted here: `1.5e3` becomes `0.15 × 10^4`.
fn float_form(value: f64) -> Option<Magnitude> {
    if value == 0.0 {
        return None;
    }
    let formatted = format!("{value:e}");
    let (mantissa, exponent) = formatted.split_once('e')?;
    let negative = mantissa.starts_with('-');
    let digits: Vec<u8> = mantissa
        .bytes()
        .filter(u8::is_ascii_digit)
        .collect::<Vec<u8>>();
    let exponent = exponent.parse::<i32>().ok()?.saturating_add(1);
    Some(Magnitude {
        negative,
        exponent,
        digits: strip_trailing_zeros(digits),
    })
}

/// Trailing zeros carry no value in `0.<digits>` form, and dropping them is what
/// makes `1.50` and `1.5` one set of bytes.
fn strip_trailing_zeros(mut digits: Vec<u8>) -> Vec<u8> {
    while digits.len() > 1 && digits.last() == Some(&b'0') {
        digits.pop();
    }
    digits
}

/// A digit string from any of the three numeric kinds is under forty bytes, and
/// a scale is under thirty, so saturating keeps these total without a branch
/// that cannot be reached.
fn digit_count(len: usize) -> i32 {
    i32::try_from(len).unwrap_or(i32::MAX)
}

fn digit_count_of(scale: u32) -> i32 {
    i32::try_from(scale).unwrap_or(i32::MAX)
}
