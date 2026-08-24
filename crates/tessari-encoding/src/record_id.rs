//! Encoding a record identity into a key.
//!
//! A record id is a union, and a union in a key needs a discriminant so that
//! decode knows the shape and so that ordering across variants is total. The
//! discriminants are part of the on-disk format and are fixed forever
//! (`docs/key-grammar.md` §5).
//!
//! Fixed-width variants carry no terminator, because the discriminant already
//! declares their width. Variable-width ones must be terminated: a fixed-width
//! suffix follows the id inside a record key, and without a terminator `"a"`
//! followed by a version byte above `'b'` would sort after `"ab"` — a wrong
//! order between two distinct records, raising nothing.

use tessari_types::RecordId;

use crate::error::{Error, Result};
use crate::order::{KeyReader, KeyWriter};

const INT: u8 = 0x01;
const TEXT: u8 = 0x02;
const UUID: u8 = 0x03;
const BYTES: u8 = 0x04;

/// Append a record id to a key under construction.
pub(crate) fn put(writer: &mut KeyWriter, id: &RecordId) {
    match id {
        RecordId::Int(value) => {
            writer.put_u8(INT).put_i64(*value);
        }
        RecordId::Text(value) => {
            writer.put_u8(TEXT).put_variable(value.as_bytes());
        }
        RecordId::Uuid(value) => {
            writer.put_u8(UUID).put_fixed(value);
        }
        RecordId::Bytes(value) => {
            writer.put_u8(BYTES).put_variable(value);
        }
    }
}

/// Read a record id back out of a key.
pub(crate) fn take(reader: &mut KeyReader<'_>) -> Result<RecordId> {
    let offset = reader.position();
    let discriminant = reader.take_u8()?;
    match discriminant {
        INT => Ok(RecordId::Int(reader.take_i64()?)),
        TEXT => {
            let bytes = reader.take_variable()?;
            let text = String::from_utf8(bytes).map_err(|_| Error::InvalidUtf8 {
                kind: reader.kind(),
            })?;
            Ok(RecordId::Text(text))
        }
        UUID => Ok(RecordId::Uuid(reader.take_fixed::<16>()?)),
        BYTES => Ok(RecordId::Bytes(reader.take_variable()?)),
        found => Err(Error::UnknownRecordIdKind {
            kind: reader.kind(),
            found,
            offset,
        }),
    }
}

#[cfg(test)]
mod tests {
    // Test assertions are exactly where a panic is the correct outcome; the
    // lints below target production paths.
    #![allow(clippy::panic, clippy::unwrap_used)]

    use super::*;
    use crate::kind::KeyKind;

    fn encode(id: &RecordId) -> Vec<u8> {
        let mut writer = KeyWriter::new();
        put(&mut writer, id);
        writer.finish()
    }

    fn decode(bytes: &[u8]) -> Result<RecordId> {
        let mut reader = KeyReader::new(KeyKind::Record, bytes);
        take(&mut reader)
    }

    #[test]
    fn discriminants_match_the_documented_values() {
        assert_eq!(encode(&RecordId::Int(0))[0], 0x01);
        assert_eq!(encode(&RecordId::from("x"))[0], 0x02);
        assert_eq!(encode(&RecordId::Uuid([0; 16]))[0], 0x03);
        assert_eq!(encode(&RecordId::Bytes(Vec::new()))[0], 0x04);
    }

    #[test]
    fn variant_order_on_disk_matches_variant_order_in_memory() {
        let ids = [
            RecordId::Int(i64::MAX),
            RecordId::from(""),
            RecordId::Uuid([0; 16]),
            RecordId::Bytes(Vec::new()),
        ];
        for pair in ids.windows(2) {
            assert!(pair[0] < pair[1]);
            assert!(encode(&pair[0]) < encode(&pair[1]));
        }
    }

    #[test]
    fn integer_ids_sort_numerically_including_negatives() {
        let values = [i64::MIN, -2, -1, 0, 1, 2, i64::MAX];
        for pair in values.windows(2) {
            assert!(
                encode(&RecordId::Int(pair[0])) < encode(&RecordId::Int(pair[1])),
                "{} should encode below {}",
                pair[0],
                pair[1]
            );
        }
    }

    #[test]
    fn text_ids_sort_lexicographically() {
        let values = ["", "a", "aa", "ab", "b"];
        for pair in values.windows(2) {
            assert!(
                encode(&RecordId::from(pair[0])) < encode(&RecordId::from(pair[1])),
                "{:?} should encode below {:?}",
                pair[0],
                pair[1]
            );
        }
    }

    #[test]
    fn every_variant_round_trips() {
        let ids = [
            RecordId::Int(-1),
            RecordId::from("with\u{0}zero"),
            RecordId::Uuid([0xab; 16]),
            RecordId::Bytes(vec![0, 1, 0, 2]),
        ];
        for id in ids {
            assert_eq!(decode(&encode(&id)).unwrap(), id);
        }
    }

    #[test]
    fn an_unknown_discriminant_is_reported_with_its_offset() {
        match decode(&[0x77, 0x00]).unwrap_err() {
            Error::UnknownRecordIdKind { found, offset, .. } => {
                assert_eq!(found, 0x77);
                assert_eq!(offset, 0);
            }
            other => panic!("unexpected error: {other}"),
        }
    }

    #[test]
    fn invalid_utf8_in_a_text_id_is_refused() {
        let mut writer = KeyWriter::new();
        writer.put_u8(TEXT).put_variable(&[0xff, 0xfe]);
        let bytes = writer.finish();
        assert!(matches!(
            decode(&bytes).unwrap_err(),
            Error::InvalidUtf8 { .. }
        ));
    }
}
