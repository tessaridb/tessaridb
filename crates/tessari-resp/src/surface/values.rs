//! Bytes a client sends, as the store's values, and back (ADR-0129 D9).
//!
//! A RESP value is bytes and nothing else. Stored as bytes it would be opaque
//! to the store, so `INCR` could not be the store's own atomic increment and a
//! value written from TessariQL could not be read here. So a value is stored
//! as the narrowest thing its bytes spell exactly: an integer when they are the
//! integer's own canonical digits, text when they are UTF-8, bytes otherwise —
//! and each comes back as the bytes it arrived as, because the canonical
//! digits of an integer are the only bytes it could have been written from.

use tessari_types::{Number, RecordId, Value};

/// A key: text when it is UTF-8, bytes otherwise — a record identity either way.
pub(crate) fn key(bytes: &[u8]) -> Value {
    match std::str::from_utf8(bytes) {
        Ok(text) => Value::String(text.to_owned()),
        Err(_) => Value::Bytes(bytes.to_vec()),
    }
}

/// What a value's bytes are stored as.
pub(crate) fn stored(bytes: &[u8]) -> Value {
    if let Some(integer) = canonical_integer(bytes) {
        return Value::Number(Number::Integer(integer));
    }
    match std::str::from_utf8(bytes) {
        Ok(text) => Value::String(text.to_owned()),
        Err(_) => Value::Bytes(bytes.to_vec()),
    }
}

/// The bytes a stored value reads back as, or `None` for a kind no string
/// stands for — a record, an array, a shape written from TessariQL.
pub(crate) fn rendered(value: &Value) -> Option<Vec<u8>> {
    match value {
        Value::String(text) => Some(text.clone().into_bytes()),
        Value::Bytes(bytes) => Some(bytes.clone()),
        Value::Number(number) => Some(number.to_string().into_bytes()),
        _ => None,
    }
}

/// A key's bytes as a client is given them.
pub(crate) fn identity(id: &RecordId) -> Vec<u8> {
    match id {
        RecordId::Text(text) => text.clone().into_bytes(),
        RecordId::Bytes(bytes) => bytes.clone(),
        RecordId::Int(integer) => integer.to_string().into_bytes(),
        RecordId::Uuid(bytes) => uuid(bytes).into_bytes(),
    }
}

/// An identity as the value `AFTER` takes to resume past it.
pub(crate) fn resume_at(id: &RecordId) -> Value {
    match id {
        RecordId::Text(text) => Value::String(text.clone()),
        RecordId::Bytes(bytes) => Value::Bytes(bytes.clone()),
        RecordId::Int(integer) => Value::Number(Number::Integer(*integer)),
        RecordId::Uuid(bytes) => Value::Uuid(*bytes),
    }
}

/// `bytes` as a signed 64-bit integer when they are its canonical decimal
/// digits: no sign but `-`, no leading zero, no `-0`.
pub(crate) fn canonical_integer(bytes: &[u8]) -> Option<i64> {
    let digits = bytes.strip_prefix(b"-").unwrap_or(bytes);
    let canonical = match digits {
        [] => false,
        [b'0'] => bytes.len() == 1,
        [first, ..] => (b'1'..=b'9').contains(first) && digits.iter().all(u8::is_ascii_digit),
    };
    if !canonical {
        return None;
    }
    std::str::from_utf8(bytes).ok()?.parse().ok()
}

/// `bytes` as a float the way a client writes one: decimal digits, a sign, a
/// point, an exponent — no spaces, nothing that is not finite.
pub(crate) fn float(bytes: &[u8]) -> Option<f64> {
    let text = std::str::from_utf8(bytes).ok()?;
    if text.is_empty() || text.bytes().any(|byte| byte.is_ascii_whitespace()) {
        return None;
    }
    let parsed = text.parse::<f64>().ok()?;
    parsed.is_finite().then_some(parsed)
}

/// A float as the shortest digits that read back as it.
pub(crate) fn float_text(value: f64) -> Vec<u8> {
    format!("{value}").into_bytes()
}

fn uuid(bytes: &[u8; 16]) -> String {
    let mut text = String::with_capacity(36);
    for (at, byte) in bytes.iter().enumerate() {
        if matches!(at, 4 | 6 | 8 | 10) {
            text.push('-');
        }
        text.push_str(&format!("{byte:02x}"));
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_value_is_stored_as_the_narrowest_kind_its_bytes_spell_exactly() {
        assert_eq!(stored(b"42"), Value::Number(Number::Integer(42)));
        assert_eq!(stored(b"-7"), Value::Number(Number::Integer(-7)));
        assert_eq!(stored(b"0"), Value::Number(Number::Integer(0)));
        for text in [
            "007",
            "-0",
            "+5",
            "1.5",
            " 1",
            "9223372036854775808",
            "",
            "-",
        ] {
            assert_eq!(
                stored(text.as_bytes()),
                Value::String(text.to_owned()),
                "{text}"
            );
        }
        assert_eq!(stored(&[0xff, 0x00]), Value::Bytes(vec![0xff, 0x00]));
        for bytes in [&b"42"[..], b"-7", b"007", b"hello", &[0xff, 0x00]] {
            assert_eq!(rendered(&stored(bytes)).unwrap(), bytes, "round trip");
        }
        assert_eq!(rendered(&Value::Array(Vec::new())), None);
    }

    #[test]
    fn a_key_is_text_when_it_can_be() {
        assert_eq!(key(b"user:1"), Value::String("user:1".to_owned()));
        assert_eq!(key(&[0xff]), Value::Bytes(vec![0xff]));
        assert_eq!(identity(&RecordId::Int(5)), b"5");
        assert_eq!(
            identity(&RecordId::Uuid([0xab; 16])),
            b"abababab-abab-abab-abab-abababababab"
        );
    }

    #[test]
    fn a_float_reads_as_a_client_writes_one() {
        assert_eq!(float(b"10.5"), Some(10.5));
        assert_eq!(float(b"-1e3"), Some(-1000.0));
        for refused in ["", " 1", "inf", "nan", "1x"] {
            assert_eq!(float(refused.as_bytes()), None, "{refused}");
        }
        assert_eq!(float_text(3.0), b"3");
        assert_eq!(float_text(10.5), b"10.5");
    }
}
