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

use tessari_kv::Value;
use tessari_types::{DatabaseId, NamespaceId, RecordId, Sequence, TableId};

use crate::error::{Error, Result};
use crate::order::{KeyReader, KeyWriter};

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
pub(crate) fn split_header(bytes: &[u8], allowed_flags: u8) -> Result<(u8, &[u8])> {
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
pub(crate) fn with_header(flags: u8, payload_len: usize) -> Vec<u8> {
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

/// One mutation inside a log record: which record, and what it becomes.
///
/// It carries the record's **address**, not its encoded key. An encoded key has
/// the version baked into it, so a record whose embedded version disagreed with
/// its own log sequence would create a second ordering authority — the thing
/// ADR-0001 exists to prevent. Carrying the address and deriving the version
/// from the log entry's own sequence makes that disagreement unrepresentable
/// rather than merely forbidden.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Mutation {
    /// The namespace the record belongs to.
    pub namespace: NamespaceId,
    /// The database within that namespace.
    pub database: DatabaseId,
    /// The table within that database.
    pub table: TableId,
    /// The record's identity within the table.
    pub id: RecordId,
    /// What the record becomes at this sequence.
    pub value: RecordValue,
}

/// Everything one commit changed, as the log carries it.
///
/// Mutations are stored in address order. That is not tidiness: a deterministic
/// apply has to be fed a deterministic record, so the *encoder* is bound by the
/// same no-unordered-iteration rule the apply path is.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct LogRecord {
    mutations: Vec<Mutation>,
}

impl LogRecord {
    /// Build a record from mutations that are already in address order.
    ///
    /// Sorting happens here rather than being assumed, because a caller that
    /// hands over an unordered collection would produce a record that replays to
    /// the same state but not to the same bytes — and byte-identical replay is
    /// the property being protected.
    #[must_use]
    pub fn new(mut mutations: Vec<Mutation>) -> Self {
        mutations.sort_by(|left, right| {
            (left.namespace, left.database, left.table, &left.id).cmp(&(
                right.namespace,
                right.database,
                right.table,
                &right.id,
            ))
        });
        Self { mutations }
    }

    /// The mutations this record applies, in address order.
    #[must_use]
    pub fn mutations(&self) -> &[Mutation] {
        &self.mutations
    }
}

impl StoreValue for LogRecord {
    /// There is deliberately no mutation count in front of the mutations.
    ///
    /// Every mutation is self-delimiting — the record id is terminated and the
    /// value is length-prefixed — so a count would be a second statement of the
    /// same fact, which is a thing that can disagree with itself. It would also
    /// need a width, and a width needs a policy for what happens when a commit
    /// exceeds it. Neither question has to be answered if the field does not
    /// exist.
    fn encode(&self) -> Value {
        let mut writer = KeyWriter::with_capacity(self.mutations.len().saturating_mul(32));
        for mutation in &self.mutations {
            writer
                .put_u32(mutation.namespace.get())
                .put_u32(mutation.database.get())
                .put_u32(mutation.table.get());
            crate::record_id::put(&mut writer, &mutation.id);
            let encoded = mutation.value.encode();
            writer
                .put_u32(length_of(encoded.as_slice()))
                .put_fixed(encoded.as_slice());
        }
        let payload = writer.finish();
        let mut buffer = with_header(0, payload.len());
        buffer.extend_from_slice(&payload);
        Value::from(buffer)
    }

    fn decode(bytes: &[u8]) -> Result<Self> {
        let (_, payload) = split_header(bytes, 0)?;
        // The reader is the crate's bounds-checked byte cursor. The kind it is
        // built with only names the entity in a truncation error, and no kind
        // tag is consumed here — this is a value payload, not a key.
        let mut reader = KeyReader::new(crate::kind::KeyKind::LogEntry, payload);
        let mut mutations = Vec::new();
        while reader.remaining() > 0 {
            let namespace = NamespaceId::new(reader.take_u32()?);
            let database = DatabaseId::new(reader.take_u32()?);
            let table = TableId::new(reader.take_u32()?);
            let id = crate::record_id::take(&mut reader)?;
            let len = reader.take_u32()?;
            let encoded = reader.take_exact(usize::try_from(len).unwrap_or(usize::MAX))?;
            mutations.push(Mutation {
                namespace,
                database,
                table,
                id,
                value: RecordValue::decode(&encoded)?,
            });
        }
        Ok(Self { mutations })
    }
}

/// A byte length as it is written into a log record.
///
/// A value longer than a `u32` can express is not something this store can
/// produce — a single record that large would have failed on memory long before
/// reaching the codec — and saturating here keeps the encoder total rather than
/// making every caller of `encode` handle a case that cannot arise. The decoder
/// would reject the result as truncated, so the failure is loud either way.
fn length_of(bytes: &[u8]) -> u32 {
    u32::try_from(bytes.len()).unwrap_or(u32::MAX)
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

    fn mutation(id: RecordId, value: RecordValue) -> Mutation {
        Mutation {
            namespace: NamespaceId::new(1),
            database: DatabaseId::new(2),
            table: TableId::new(3),
            id,
            value,
        }
    }

    #[test]
    fn a_log_record_round_trips_every_mutation_shape() {
        let record = LogRecord::new(vec![
            mutation(RecordId::Int(-42), RecordValue::Present(b"a".to_vec())),
            mutation(RecordId::from("text"), RecordValue::Tombstone),
            mutation(RecordId::Uuid([0x5a; 16]), RecordValue::Present(Vec::new())),
            mutation(
                RecordId::Bytes(vec![0x00, 0xff, 0x00]),
                RecordValue::Present(vec![0x00; 300]),
            ),
        ]);
        let encoded = record.encode();
        assert_eq!(encoded.as_slice()[0], CODEC_VERSION);
        assert_eq!(LogRecord::decode(encoded.as_slice()).unwrap(), record);
    }

    #[test]
    fn an_empty_log_record_round_trips_as_the_header_alone() {
        let record = LogRecord::new(Vec::new());
        assert!(record.mutations().is_empty());
        let encoded = record.encode();
        assert_eq!(encoded.as_slice(), &[CODEC_VERSION, 0]);
        assert_eq!(LogRecord::decode(encoded.as_slice()).unwrap(), record);
    }

    #[test]
    fn mutations_are_stored_in_address_order_whatever_order_they_arrive_in() {
        // Byte-identical replay depends on the encoder being deterministic, not
        // only on the apply path being deterministic. A caller that hands over
        // an unordered collection must not be able to change the bytes.
        let ordered = LogRecord::new(vec![
            mutation(RecordId::from("a"), RecordValue::Tombstone),
            mutation(RecordId::from("b"), RecordValue::Tombstone),
            mutation(RecordId::from("c"), RecordValue::Tombstone),
        ]);
        let shuffled = LogRecord::new(vec![
            mutation(RecordId::from("c"), RecordValue::Tombstone),
            mutation(RecordId::from("a"), RecordValue::Tombstone),
            mutation(RecordId::from("b"), RecordValue::Tombstone),
        ]);
        assert_eq!(ordered, shuffled);
        assert_eq!(
            ordered.encode().as_slice(),
            shuffled.encode().as_slice(),
            "the same mutation set must encode to the same bytes"
        );
    }

    #[test]
    fn mutations_sort_by_the_whole_address_and_not_only_by_record_id() {
        let record = LogRecord::new(vec![
            Mutation {
                namespace: NamespaceId::new(2),
                database: DatabaseId::new(1),
                table: TableId::new(1),
                id: RecordId::from("a"),
                value: RecordValue::Tombstone,
            },
            Mutation {
                namespace: NamespaceId::new(1),
                database: DatabaseId::new(1),
                table: TableId::new(1),
                id: RecordId::from("z"),
                value: RecordValue::Tombstone,
            },
        ]);
        assert_eq!(record.mutations()[0].namespace, NamespaceId::new(1));
    }

    #[test]
    fn a_log_record_truncated_mid_mutation_is_refused_rather_than_half_decoded() {
        let record = LogRecord::new(vec![mutation(
            RecordId::from("r"),
            RecordValue::Present(b"payload".to_vec()),
        )]);
        let full = record.encode();
        let bytes = full.as_slice();
        // From one byte past the header: a cut exactly at the header is not a
        // truncated record, it is an empty one, and that is legitimate.
        for cut in HEADER_LEN.saturating_add(1)..bytes.len() {
            assert!(
                LogRecord::decode(&bytes[..cut]).is_err(),
                "a record cut at {cut} decoded anyway"
            );
        }
    }

    #[test]
    fn a_value_length_naming_more_bytes_than_exist_is_refused() {
        // The length comes out of the payload, so it is not trusted.
        let mut bytes = LogRecord::new(vec![mutation(
            RecordId::from("r"),
            RecordValue::Present(b"x".to_vec()),
        )])
        .encode()
        .into_bytes();
        let last = bytes.len().saturating_sub(3);
        bytes[last] = 0xff;
        assert!(LogRecord::decode(&bytes).is_err());
    }
}
