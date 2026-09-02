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

impl RecordId {
    /// The identity as the language spells one, so it can be written back.
    ///
    /// # Why this is not `Display`
    ///
    /// `Display` is a *rendering* — the form an error message and a log line
    /// want, where a UUID's thirty-two undivided digits are shorter and nothing
    /// reads them back. That convention is stated for the same type family in
    /// this crate's `text` module, and it is left alone: every message that
    /// names a record keeps the wording it has.
    ///
    /// This is the other form, and the two are genuinely different text. An
    /// identity stands in the grammar as one of `1`, `'ada'`, `uuid '…'` or
    /// `0x…`, and nothing else — so `Display`'s bare hex does not lex as an
    /// identity at all, and `Display`'s unquoted text lexes as a different one.
    ///
    /// # What depends on it
    ///
    /// This is the single spelling the store answers with: the protocol says
    /// record identities are text "exactly as the store spells them", and that a
    /// client naming a record "writes that text into its next script". A caller
    /// that pastes what it was given must land on the record it was given.
    #[must_use]
    pub fn to_literal(&self) -> String {
        match self {
            Self::Int(value) => value.to_string(),
            Self::Text(value) => crate::string_to_literal(value),
            Self::Uuid(bytes) => format!("uuid '{}'", crate::uuid_to_text(bytes)),
            // The one variant whose `Display` was already a literal, because
            // `0x` is how the language writes bytes and always was.
            Self::Bytes(_) => self.to_string(),
        }
    }
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
