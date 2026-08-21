//! Failures raised while encoding or decoding stored bytes.
//!
//! Every variant names the key kind or the value it was reading. An error that
//! reports opaque bytes tells an operator that something is wrong and nothing
//! about which subsystem wrote it, which is the difference between a diagnosable
//! failure and an unexplained one.
//!
//! Categories come from the substrate's taxonomy rather than a second one:
//! callers already branch on [`ErrorCategory`], and two parallel vocabularies
//! for the same question is how a caller ends up handling only one of them.

use bgv_db_kv::ErrorCategory;

use crate::kind::KeyKind;

/// Result alias for every fallible operation in this crate.
pub type Result<T> = std::result::Result<T, Error>;

/// A failure encoding or decoding a key or a value.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum Error {
    /// The bytes ran out before the component did.
    #[error(
        "{kind} key is truncated at offset {offset}: {needed} more byte(s) required, {available} available"
    )]
    Truncated {
        /// The kind being decoded.
        kind: KeyKind,
        /// Where the read started.
        offset: usize,
        /// How many bytes the component needed.
        needed: usize,
        /// How many were left.
        available: usize,
    },

    /// The component was fully read but bytes remained after it.
    #[error("{kind} key has {extra} unexpected trailing byte(s)")]
    TrailingBytes {
        /// The kind being decoded.
        kind: KeyKind,
        /// How many bytes were left over.
        extra: usize,
    },

    /// A variable-length component never reached its terminator.
    #[error("{kind} key has an unterminated variable-length component starting at offset {offset}")]
    UnterminatedComponent {
        /// The kind being decoded.
        kind: KeyKind,
        /// Where the component started.
        offset: usize,
    },

    /// An escape byte was followed by something that is neither an escaped zero
    /// nor a terminator.
    #[error("{kind} key has an invalid escape sequence 0x00 0x{found:02x} at offset {offset}")]
    InvalidEscape {
        /// The kind being decoded.
        kind: KeyKind,
        /// Where the escape byte sat.
        offset: usize,
        /// The byte that followed it.
        found: u8,
    },

    /// The leading byte does not name the kind the caller asked to decode.
    #[error("expected a {expected} key, found kind tag 0x{found:02x} ({found_name})")]
    UnexpectedKind {
        /// What the caller was decoding.
        expected: KeyKind,
        /// The tag actually present.
        found: u8,
        /// The name of that tag, or `unassigned`.
        found_name: &'static str,
    },

    /// A record id carried a discriminant this build does not know.
    #[error("{kind} key holds an unknown record-id discriminant 0x{found:02x} at offset {offset}")]
    UnknownRecordIdKind {
        /// The kind being decoded.
        kind: KeyKind,
        /// The discriminant found.
        found: u8,
        /// Where it sat.
        offset: usize,
    },

    /// A text record id did not decode as UTF-8.
    #[error("{kind} key holds a text record id that is not valid UTF-8")]
    InvalidUtf8 {
        /// The kind being decoded.
        kind: KeyKind,
    },

    /// A stored value was shorter than its own header.
    #[error("stored value is truncated: {len} byte(s) present, at least {needed} required")]
    ValueTruncated {
        /// How many bytes were present.
        len: usize,
        /// How many the header needs.
        needed: usize,
    },

    /// The value's codec version is not one this build can read.
    ///
    /// The data is intact; this binary is the wrong one. Opening
    /// forward-compatibly by ignoring the version would write old-format bytes
    /// into a newer store, so it is refused instead.
    #[error("stored value has codec version {found}, this build supports {supported}")]
    UnsupportedCodecVersion {
        /// The version on disk.
        found: u8,
        /// The version this build writes and reads.
        supported: u8,
    },

    /// A reserved flag bit was set.
    ///
    /// Ignoring it would misread a record whose writer knew something this build
    /// does not.
    #[error("stored value has reserved flag bits set (flags 0x{flags:02x})")]
    ReservedFlags {
        /// The flags byte as found.
        flags: u8,
    },

    /// A tombstone carried a payload, which contradicts its own meaning.
    #[error("stored value is a tombstone but carries {len} payload byte(s)")]
    TombstoneWithPayload {
        /// How many payload bytes followed the header.
        len: usize,
    },

    /// A payload carried a value type this build does not know.
    ///
    /// Not corruption: the bytes are well-formed and a newer build would read
    /// them. Guessing a type from the bytes that follow would silently produce
    /// a wrong value, which is worse than refusing.
    #[error("stored value carries unknown type tag 0x{tag:02x}")]
    UnknownValueTag {
        /// The tag that was found.
        tag: u8,
    },

    /// An index key carries a value tag this build does not know.
    ///
    /// Separate from [`Error::UnknownValueTag`] because the two tables are
    /// separate contracts even though they carry the same numbers: one orders,
    /// the other round-trips, and a message that named the wrong one would send
    /// an operator to the wrong file.
    #[error("{kind} key holds an unknown index value tag 0x{tag:02x}")]
    UnknownIndexTag {
        /// The kind being decoded.
        kind: KeyKind,
        /// The tag that was found.
        tag: u8,
    },

    /// A decimal's mantissa and scale do not describe a representable number.
    #[error("stored decimal has mantissa {mantissa} and scale {scale}, which is out of range")]
    InvalidDecimal {
        /// The unscaled value read.
        mantissa: i128,
        /// The number of fractional digits read.
        scale: u32,
    },

    /// A sub-second remainder was a whole second or more.
    #[error("stored time has a sub-second remainder of {nanos}, which is a whole second or more")]
    InvalidSubSecond {
        /// The remainder read.
        nanos: u32,
    },

    /// The store's on-disk format version is newer than this build supports.
    #[error("store on-disk format version is {found}, this build supports up to {supported}")]
    UnsupportedFormatVersion {
        /// The version read from the store.
        found: u32,
        /// The highest version this build understands.
        supported: u32,
    },
}

impl Error {
    /// The category this error belongs to.
    ///
    /// Malformed stored bytes are `corruption`; bytes that are well-formed but
    /// written by a newer format are `incompatible`, because the operator action
    /// is to deploy a different binary rather than to repair data.
    #[must_use]
    pub const fn category(&self) -> ErrorCategory {
        match self {
            Self::Truncated { .. }
            | Self::TrailingBytes { .. }
            | Self::UnterminatedComponent { .. }
            | Self::InvalidEscape { .. }
            | Self::UnexpectedKind { .. }
            | Self::UnknownRecordIdKind { .. }
            | Self::InvalidUtf8 { .. }
            | Self::ValueTruncated { .. }
            | Self::TombstoneWithPayload { .. }
            | Self::InvalidDecimal { .. }
            | Self::InvalidSubSecond { .. } => ErrorCategory::Corruption,
            Self::UnsupportedCodecVersion { .. }
            | Self::ReservedFlags { .. }
            | Self::UnknownValueTag { .. }
            | Self::UnknownIndexTag { .. }
            | Self::UnsupportedFormatVersion { .. } => ErrorCategory::Incompatible,
        }
    }

    /// Stable machine-readable code for this error.
    #[must_use]
    pub const fn code(&self) -> &'static str {
        self.category().code()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn malformed_bytes_are_corruption_and_old_binaries_are_incompatible() {
        let corrupt = Error::TrailingBytes {
            kind: KeyKind::Record,
            extra: 3,
        };
        assert_eq!(corrupt.category(), ErrorCategory::Corruption);
        assert_eq!(corrupt.code(), "corruption");

        let incompatible = Error::UnsupportedCodecVersion {
            found: 9,
            supported: 1,
        };
        assert_eq!(incompatible.category(), ErrorCategory::Incompatible);
        assert!(!incompatible.category().is_retryable());
    }

    #[test]
    fn messages_name_the_kind_rather_than_dumping_bytes() {
        let error = Error::UnterminatedComponent {
            kind: KeyKind::Record,
            offset: 13,
        };
        let text = error.to_string();
        assert!(text.contains("record"), "{text}");
        assert!(text.contains("13"), "{text}");
    }
}
