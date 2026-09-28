//! Numbers, bounds, times and bytes in a stored record.

use super::{bound_kind, number_kind, put_value, take_value};
use crate::error::{Error, Result};
use crate::order::{KeyReader, KeyWriter};
use rust_decimal::Decimal;
use std::ops::Bound;
use tessari_types::{Number, Value};

pub(crate) fn put_number(writer: &mut KeyWriter, number: &Number) {
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

pub(crate) fn take_number(reader: &mut KeyReader<'_>) -> Result<Number> {
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

pub(crate) fn put_bound(writer: &mut KeyWriter, bound: &Bound<Value>) {
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

pub(crate) fn take_bound(reader: &mut KeyReader<'_>, depth: usize) -> Result<Bound<Value>> {
    match reader.take_u8()? {
        bound_kind::UNBOUNDED => Ok(Bound::Unbounded),
        bound_kind::INCLUDED => Ok(Bound::Included(take_value(reader, depth)?)),
        bound_kind::EXCLUDED => Ok(Bound::Excluded(take_value(reader, depth)?)),
        unknown => Err(Error::UnknownValueTag { tag: unknown }),
    }
}

pub(crate) fn take_time(reader: &mut KeyReader<'_>) -> Result<(i64, u32)> {
    Ok((reader.take_i64()?, reader.take_u32()?))
}

pub(crate) fn put_bytes(writer: &mut KeyWriter, bytes: &[u8]) {
    writer.put_u32(count_of(bytes.len())).put_fixed(bytes);
}

pub(crate) fn take_bytes(reader: &mut KeyReader<'_>) -> Result<Vec<u8>> {
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
pub(crate) fn count_of(len: usize) -> u32 {
    u32::try_from(len).unwrap_or(u32::MAX)
}
