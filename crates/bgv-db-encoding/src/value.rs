//! The value codec.
//!
//! Every stored value starts with a codec version. The first byte is format
//! metadata, not payload, and decode dispatches on it. Storing a bare serialized
//! struct instead and relying on the serializer's own tolerance works until the
//! day the format changes, at which point old bytes decode into a plausible
//! wrong value with no error.
//!
//! Layout, uniform across every value in the store:
//!
//! ```text
//! <codec-version:1> <flags:1> <payload…>
//! ```
//!
//! Flag bits that a value type does not define are reserved and must be zero. A
//! set reserved bit means the writer knew something this build does not, so it
//! is an error rather than something to ignore.

use bgv_db_kv::Value;
use bgv_db_types::Sequence;

use crate::error::{Error, Result};

/// The codec version this build writes and reads.
pub const CODEC_VERSION: u8 = 1;

/// Bit 0 of the flags byte: the record was deleted at this version.
const FLAG_TOMBSTONE: u8 = 0b0000_0001;

/// Bytes of header that precede every payload.
const HEADER_LEN: usize = 2;

/// A value that can be stored under a [`StoreKey`](crate::StoreKey).
pub trait StoreValue: Sized {
    /// Encode to the bytes written into the substrate.
    fn encode(&self) -> Value;

    /// Decode from bytes read out of the substrate.
    ///
    /// # Errors
    ///
    /// Returns an error when the bytes are truncated, carry an unsupported
    /// codec version, or set a reserved flag bit.
    fn decode(bytes: &[u8]) -> Result<Self>;
}

/// Split the common header off a stored value.
fn split_header(bytes: &[u8], allowed_flags: u8) -> Result<(u8, &[u8])> {
    let header = bytes.get(..HEADER_LEN).ok_or(Error::ValueTruncated {
        len: bytes.len(),
        needed: HEADER_LEN,
    })?;
    let version = header[0];
    if version != CODEC_VERSION {
        return Err(Error::UnsupportedCodecVersion {
            found: version,
            supported: CODEC_VERSION,
        });
    }
    let flags = header[1];
    if flags & !allowed_flags != 0 {
        return Err(Error::ReservedFlags { flags });
    }
    let payload = bytes.get(HEADER_LEN..).unwrap_or_default();
    Ok((flags, payload))
}

/// Write the common header into a fresh buffer.
fn with_header(flags: u8, payload_len: usize) -> Vec<u8> {
    let mut buffer = Vec::with_capacity(HEADER_LEN.saturating_add(payload_len));
    buffer.push(CODEC_VERSION);
    buffer.push(flags);
    buffer
}

/// One version of one record.
///
/// A deletion is a version, not an absence: under MVCC a reader at an older
/// sequence must still see the record, and a reader at a newer one must see that
/// it is gone. An absent key cannot express either.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RecordValue {
    /// The record existed at this version, carrying this payload.
    ///
    /// The payload is opaque here — the document codec owns its interior.
    Present(Vec<u8>),
    /// The record was deleted at this version.
    Tombstone,
}

impl RecordValue {
    /// Whether this version marks a deletion.
    #[must_use]
    pub const fn is_tombstone(&self) -> bool {
        matches!(self, Self::Tombstone)
    }

    /// The payload, or an empty slice for a tombstone.
    #[must_use]
    pub fn payload(&self) -> &[u8] {
        match self {
            Self::Present(payload) => payload,
            Self::Tombstone => &[],
        }
    }
}

impl StoreValue for RecordValue {
    fn encode(&self) -> Value {
        match self {
            Self::Present(payload) => {
                let mut buffer = with_header(0, payload.len());
                buffer.extend_from_slice(payload);
                Value::from(buffer)
            }
            Self::Tombstone => Value::from(with_header(FLAG_TOMBSTONE, 0)),
        }
    }

    fn decode(bytes: &[u8]) -> Result<Self> {
        let (flags, payload) = split_header(bytes, FLAG_TOMBSTONE)?;
        if flags & FLAG_TOMBSTONE == 0 {
            return Ok(Self::Present(payload.to_vec()));
        }
        if payload.is_empty() {
            return Ok(Self::Tombstone);
        }
        Err(Error::TombstoneWithPayload { len: payload.len() })
    }
}

/// The store's own on-disk format version.
///
/// Independent of any storage engine's internal versioning: this one describes
/// the key grammar and the value codec, which are ours.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct FormatVersion(u32);

impl FormatVersion {
    /// The format this build writes.
    pub const CURRENT: Self = Self(1);

    /// Wrap a raw format version.
    #[must_use]
    pub const fn new(value: u32) -> Self {
        Self(value)
    }

    /// The raw format version.
    #[must_use]
    pub const fn get(self) -> u32 {
        self.0
    }

    /// Refuse a store written by a newer build.
    ///
    /// Opening forward-compatibly would write this build's format into a store
    /// that already holds a newer one, which is not recoverable afterwards.
    ///
    /// # Errors
    ///
    /// Returns [`Error::UnsupportedFormatVersion`] when the store is newer than
    /// this build.
    pub fn check_supported(self) -> Result<()> {
        if self.0 > Self::CURRENT.0 {
            return Err(Error::UnsupportedFormatVersion {
                found: self.0,
                supported: Self::CURRENT.0,
            });
        }
        Ok(())
    }
}

impl StoreValue for FormatVersion {
    fn encode(&self) -> Value {
        let mut buffer = with_header(0, 4);
        buffer.extend_from_slice(&self.0.to_be_bytes());
        Value::from(buffer)
    }

    fn decode(bytes: &[u8]) -> Result<Self> {
        let (_, payload) = split_header(bytes, 0)?;
        let raw: [u8; 4] = payload.try_into().map_err(|_| Error::ValueTruncated {
            len: bytes.len(),
            needed: HEADER_LEN.saturating_add(4),
        })?;
        Ok(Self(u32::from_be_bytes(raw)))
    }
}

impl StoreValue for Sequence {
    fn encode(&self) -> Value {
        let mut buffer = with_header(0, 8);
        buffer.extend_from_slice(&self.get().to_be_bytes());
        Value::from(buffer)
    }

    fn decode(bytes: &[u8]) -> Result<Self> {
        let (_, payload) = split_header(bytes, 0)?;
        let raw: [u8; 8] = payload.try_into().map_err(|_| Error::ValueTruncated {
            len: bytes.len(),
            needed: HEADER_LEN.saturating_add(8),
        })?;
        Ok(Self::new(u64::from_be_bytes(raw)))
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    #[test]
    fn a_present_record_round_trips_its_payload() {
        let value = RecordValue::Present(b"payload".to_vec());
        let encoded = value.encode();
        assert_eq!(encoded.as_slice()[0], CODEC_VERSION);
        assert_eq!(RecordValue::decode(encoded.as_slice()).unwrap(), value);
    }

    #[test]
    fn a_tombstone_is_a_version_with_no_payload() {
        let encoded = RecordValue::Tombstone.encode();
        assert_eq!(encoded.as_slice(), &[CODEC_VERSION, FLAG_TOMBSTONE]);
        let decoded = RecordValue::decode(encoded.as_slice()).unwrap();
        assert!(decoded.is_tombstone());
        assert!(decoded.payload().is_empty());
    }

    #[test]
    fn an_empty_present_record_is_not_a_tombstone() {
        let encoded = RecordValue::Present(Vec::new()).encode();
        let decoded = RecordValue::decode(encoded.as_slice()).unwrap();
        assert!(!decoded.is_tombstone());
    }

    #[test]
    fn an_unknown_codec_version_is_refused_rather_than_guessed() {
        let error = RecordValue::decode(&[9, 0]).unwrap_err();
        assert!(matches!(
            error,
            Error::UnsupportedCodecVersion {
                found: 9,
                supported: CODEC_VERSION
            }
        ));
        assert_eq!(error.code(), "incompatible");
    }

    #[test]
    fn a_reserved_flag_bit_is_refused() {
        let error = RecordValue::decode(&[CODEC_VERSION, 0b0000_0010]).unwrap_err();
        assert!(matches!(error, Error::ReservedFlags { flags: 0b10 }));
    }

    #[test]
    fn a_tombstone_bit_on_a_meta_value_is_reserved_there() {
        let error =
            FormatVersion::decode(&[CODEC_VERSION, FLAG_TOMBSTONE, 0, 0, 0, 1]).unwrap_err();
        assert!(matches!(error, Error::ReservedFlags { .. }));
    }

    #[test]
    fn a_tombstone_carrying_payload_contradicts_itself() {
        let error = RecordValue::decode(&[CODEC_VERSION, FLAG_TOMBSTONE, 0xAA]).unwrap_err();
        assert!(matches!(error, Error::TombstoneWithPayload { len: 1 }));
    }

    #[test]
    fn a_value_shorter_than_its_header_is_truncated() {
        assert!(matches!(
            RecordValue::decode(&[CODEC_VERSION]).unwrap_err(),
            Error::ValueTruncated { len: 1, needed: 2 }
        ));
    }

    #[test]
    fn the_format_version_round_trips_and_refuses_newer_stores() {
        let encoded = FormatVersion::CURRENT.encode();
        assert_eq!(
            FormatVersion::decode(encoded.as_slice()).unwrap(),
            FormatVersion::CURRENT
        );
        assert!(FormatVersion::CURRENT.check_supported().is_ok());
        assert!(matches!(
            FormatVersion::new(99).check_supported().unwrap_err(),
            Error::UnsupportedFormatVersion {
                found: 99,
                supported: 1
            }
        ));
    }

    #[test]
    fn a_sequence_round_trips_as_a_stored_value() {
        let sequence = Sequence::new(1_234_567);
        let encoded = sequence.encode();
        assert_eq!(Sequence::decode(encoded.as_slice()).unwrap(), sequence);
    }
}
