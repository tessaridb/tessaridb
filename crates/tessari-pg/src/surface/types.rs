//! The correspondence between the store's values and the wire protocol's
//! type system: one object id per column, one text form per value, and the
//! type sizes the row description reports.
//!
//! Text format is the contract; every value the store holds has one exact
//! text form here, and the forms the binary protocol will add later ride the
//! same correspondence.

use tessari_types::json;
use tessaridb::Value;

/// The object ids this surface answers with — the dialect's fixed set.
pub(crate) mod oid {
    /// `bool`
    pub(crate) const BOOL: u32 = 16;
    /// `bytea`
    pub(crate) const BYTEA: u32 = 17;
    /// `int8`; every integer answers as the widest kind, which the clients
    /// of the dialect read without complaint.
    pub(crate) const INT8: u32 = 20;
    /// `int2`
    pub(crate) const INT2: u32 = 21;
    /// `int4`
    pub(crate) const INT4: u32 = 23;
    /// `text`
    pub(crate) const TEXT: u32 = 25;
    /// a character string of unspecified length
    pub(crate) const VARCHAR: u32 = 1043;
    /// an undecorated literal, the type a bare string arrives as
    pub(crate) const UNKNOWN: u32 = 705;
    /// `float4`
    pub(crate) const FLOAT4: u32 = 700;
    /// `float8`
    pub(crate) const FLOAT8: u32 = 701;
    /// `numeric`
    pub(crate) const NUMERIC: u32 = 1700;
    /// `timestamptz`; the store's datetimes are instants.
    pub(crate) const TIMESTAMPTZ: u32 = 1184;
    /// `interval`
    pub(crate) const INTERVAL: u32 = 1186;
    /// `uuid`
    pub(crate) const UUID: u32 = 2950;
    /// `jsonb`; objects, arrays, sets and ranges answer as JSON.
    pub(crate) const JSONB: u32 = 3802;
}

/// The wire size a type id reports; the variable-width ones answer -1.
pub(crate) fn size_of(type_oid: u32) -> i16 {
    match type_oid {
        oid::BOOL => 1,
        oid::INT8 => 8,
        oid::FLOAT8 => 8,
        oid::TIMESTAMPTZ => 8,
        oid::UUID => 16,
        _ => -1,
    }
}

/// The type id a value answers under, deciding a column's identity from its
/// first non-null cell; a column of nothing is text.
pub(crate) fn type_of(value: &Value) -> u32 {
    match value {
        Value::Bool(_) => oid::BOOL,
        Value::Number(tessaridb::Number::Float(_)) => oid::FLOAT8,
        Value::Number(tessaridb::Number::Decimal(_)) => oid::NUMERIC,
        Value::Number(tessaridb::Number::Integer(_)) => oid::INT8,
        Value::Bytes(_) => oid::BYTEA,
        Value::Datetime(_) => oid::TIMESTAMPTZ,
        Value::Duration(_) => oid::INTERVAL,
        Value::Uuid(_) => oid::UUID,
        Value::Object(_) | Value::Array(_) | Value::Set(_) | Value::Range(_) => oid::JSONB,
        Value::None | Value::Null => oid::TEXT,
        _ => oid::TEXT,
    }
}

/// The text form one value crosses in: `None` is the wire's NULL — the two
/// absences of the store both read as the dialect's one.
pub(crate) fn text_of(value: &Value) -> Option<String> {
    match value {
        Value::None | Value::Null => None,
        Value::Bool(held) => Some(if *held {
            "t".to_owned()
        } else {
            "f".to_owned()
        }),
        Value::Number(number) => Some(number.to_string()),
        Value::String(text) => Some(text.clone()),
        Value::Bytes(bytes) => Some(bytea(bytes)),
        Value::Datetime(held) => Some(held.to_string()),
        Value::Duration(held) => Some(held.to_string()),
        Value::Uuid(bytes) => Some(uuid(bytes)),
        Value::Object(_) | Value::Array(_) | Value::Set(_) | Value::Range(_) => {
            // Table names stay unresolved here: the catalog lookup that would
            // fill the map is a surface the BI baseline never meets, and the
            // writer's fallback spells what it cannot name.
            let names = json::Names::new();
            let mut out = String::new();
            json::write(&mut out, value, &names);
            Some(out)
        }
        Value::Table(_) | Value::Record(_) | Value::Geometry(_) | Value::Regex(_) => {
            Some(value.to_string())
        }
    }
}

/// The value a text-format parameter reads as under the type it was declared
/// with. The scalar set the wire's clients bind; anything outside it refuses
/// by name rather than reading as a string nobody asked for.
pub(crate) fn value_of_text(
    type_oid: u32,
    text: &str,
) -> Result<Value, crate::surface::errors::Refused> {
    use crate::surface::errors::{Refused, code};
    let invalid = || {
        Refused::with_detail(
            code::INVALID_TEXT,
            "the parameter's text does not read as its declared type",
            format!("type {type_oid}, value {text:?}"),
        )
    };
    match type_oid {
        oid::BOOL => match text {
            "t" | "true" | "yes" | "1" => Ok(Value::Bool(true)),
            "f" | "false" | "no" | "0" => Ok(Value::Bool(false)),
            _ => Err(invalid()),
        },
        oid::INT2 | oid::INT4 | oid::INT8 => text
            .parse::<i64>()
            .map(|held| Value::Number(held.into()))
            .map_err(|_| invalid()),
        oid::FLOAT4 | oid::FLOAT8 => text
            .parse::<f64>()
            .map(tessaridb::Number::float)
            .map(Value::Number)
            .map_err(|_| invalid()),
        oid::NUMERIC => text
            .parse::<rust_decimal::Decimal>()
            .map(tessaridb::Number::from)
            .map(Value::Number)
            .map_err(|_| invalid()),
        oid::TEXT | oid::VARCHAR | oid::UNKNOWN => Ok(Value::String(text.to_owned())),
        other => Err(Refused::with_detail(
            code::FEATURE,
            "binding this parameter type is not spoken in this build",
            format!("type {other}"),
        )),
    }
}

/// `bytea`'s text form: a `\x` prefix and the hex of the bytes.
fn bytea(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len().saturating_mul(2).saturating_add(2));
    out.push_str("\\x");
    for byte in bytes {
        let high = HEX
            .get(usize::from(*byte) >> 4)
            .copied()
            .unwrap_or_default();
        let low = HEX
            .get(usize::from(*byte) & 0x0f)
            .copied()
            .unwrap_or_default();
        out.push(char::from(high));
        out.push(char::from(low));
    }
    out
}

/// The hyphenated spelling of sixteen bytes.
fn uuid(bytes: &[u8; 16]) -> String {
    let mut out = String::with_capacity(36);
    for (i, byte) in bytes.iter().enumerate() {
        if matches!(i, 4 | 6 | 8 | 10) {
            out.push('-');
        }
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

#[cfg(test)]
mod tests;
