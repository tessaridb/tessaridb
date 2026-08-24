//! The identity of a record within its table.
//!
//! A record id is a union, because the natural identifier differs by domain: an
//! auto-incrementing integer, a slug, a UUID handed down by a caller, or opaque
//! bytes from another system. Forcing all of them into a string would work and
//! would also make `1` and `"1"` the same record, which they are not.

use core::fmt;

/// Identifies one record inside one table.
///
/// The variants are ordered, and that order is part of the on-disk format: it
/// decides where a record sorts among its neighbours. See `docs/key-grammar.md`
/// §5 — the discriminant assigned to each variant is fixed forever.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum RecordId {
    /// A signed integer identifier.
    Int(i64),
    /// A text identifier.
    Text(String),
    /// A 128-bit identifier, stored as raw bytes so no dependency is imposed on
    /// callers that already have their own UUID type.
    Uuid([u8; 16]),
    /// An opaque byte identifier.
    Bytes(Vec<u8>),
}

impl From<i64> for RecordId {
    fn from(value: i64) -> Self {
        Self::Int(value)
    }
}

impl From<String> for RecordId {
    fn from(value: String) -> Self {
        Self::Text(value)
    }
}

impl From<&str> for RecordId {
    fn from(value: &str) -> Self {
        Self::Text(value.to_owned())
    }
}

impl From<[u8; 16]> for RecordId {
    fn from(value: [u8; 16]) -> Self {
        Self::Uuid(value)
    }
}

impl fmt::Display for RecordId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Int(value) => write!(f, "{value}"),
            Self::Text(value) => write!(f, "{value}"),
            Self::Uuid(bytes) => {
                for byte in bytes {
                    write!(f, "{byte:02x}")?;
                }
                Ok(())
            }
            Self::Bytes(bytes) => {
                f.write_str("0x")?;
                for byte in bytes {
                    write!(f, "{byte:02x}")?;
                }
                Ok(())
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn variants_are_ordered_by_declaration_not_by_content() {
        // The declaration order is the on-disk order: every int precedes every
        // text, whatever the values are.
        assert!(RecordId::Int(i64::MAX) < RecordId::Text(String::new()));
        assert!(RecordId::Text("zzz".to_owned()) < RecordId::Uuid([0; 16]));
        assert!(RecordId::Uuid([0xff; 16]) < RecordId::Bytes(Vec::new()));
    }

    #[test]
    fn an_integer_and_its_text_form_are_different_records() {
        assert_ne!(RecordId::Int(1), RecordId::from("1"));
    }

    #[test]
    fn display_is_readable_per_variant() {
        assert_eq!(RecordId::Int(-4).to_string(), "-4");
        assert_eq!(RecordId::from("user").to_string(), "user");
        assert_eq!(RecordId::Uuid([0xab; 16]).to_string(), "ab".repeat(16));
        assert_eq!(RecordId::Bytes(vec![0x01, 0x02]).to_string(), "0x0102");
    }
}
