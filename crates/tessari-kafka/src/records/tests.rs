//! Record batches read from frames a 3.9 Java producer sent, one per codec.

use super::*;
use crate::api::produce::Request;
use crate::codec::{Reader, Writer};
use crate::header::RequestHeader;

/// The batches of the one partition of a captured produce request.
fn batches_of(frame: &[u8]) -> Vec<Batch> {
    let mut reader = Reader::new(frame);
    let header = RequestHeader::read(&mut reader).unwrap();
    assert_eq!((header.key, header.version, header.flexible), (0, 9, true));
    let request = Request::read(&mut reader, header.flexible).unwrap();
    assert_eq!(reader.remaining(), 0, "the whole request is read");
    assert_eq!(request.topics.len(), 1);
    assert_eq!(request.topics[0].name, "fy");
    decode_batches(request.topics[0].partitions[0].records.unwrap()).unwrap()
}

fn values(batches: &[Batch]) -> Vec<Vec<u8>> {
    batches
        .iter()
        .flat_map(|batch| batch.records.iter())
        .map(|record| record.value.clone().unwrap())
        .collect()
}

#[test]
fn a_gzip_batch_keeps_keys_headers_and_order() {
    let batches = batches_of(include_bytes!("../../fixtures/produce-v9-1.bin"));
    assert_eq!(batches.len(), 1);
    let batch = &batches[0];
    assert_eq!(batch.codec, Codec::Gzip);
    assert!(
        batch.producer_id >= 0,
        "the 3.9 producer is idempotent by default"
    );
    assert_eq!(batch.base_sequence, 0);
    let records = &batch.records;
    assert_eq!(records.len(), 2);
    assert_eq!(records[0].key.as_deref(), Some(&b"k1"[..]));
    assert_eq!(records[0].value.as_deref(), Some(&b"value-one"[..]));
    assert_eq!(
        records[0].headers,
        vec![Header {
            key: "h1".to_owned(),
            value: Some(b"v1".to_vec())
        }]
    );
    assert_eq!(records[1].key.as_deref(), Some(&b"k2"[..]));
    assert_eq!(records[1].offset_delta, 1);
    assert!(
        records[0].timestamp > 1_700_000_000_000,
        "milliseconds since the epoch"
    );
}

#[test]
fn every_codec_a_producer_uses_reads_back_its_value() {
    for (frame, codec, value) in [
        (
            &include_bytes!("../../fixtures/produce-v9-2.bin")[..],
            Codec::Snappy,
            &b"plain-snappy"[..],
        ),
        (
            &include_bytes!("../../fixtures/produce-v9-3.bin")[..],
            Codec::Lz4,
            b"plain-lz4",
        ),
        (
            &include_bytes!("../../fixtures/produce-v9-4.bin")[..],
            Codec::Zstd,
            b"plain-zstd",
        ),
        (
            &include_bytes!("../../fixtures/produce-v9-5.bin")[..],
            Codec::None,
            b"plain-none",
        ),
    ] {
        let batches = batches_of(frame);
        assert_eq!(batches[0].codec, codec);
        assert_eq!(values(&batches), vec![value.to_vec()], "{codec:?}");
        assert_eq!(batches[0].records[0].key, None);
    }
}

#[test]
fn a_flipped_byte_is_refused_by_the_checksum() {
    let mut frame = include_bytes!("../../fixtures/produce-v9-5.bin").to_vec();
    let at = frame
        .windows(10)
        .position(|window| window == b"plain-none")
        .unwrap();
    frame[at] ^= 0x01;
    let mut reader = Reader::new(&frame);
    let header = RequestHeader::read(&mut reader).unwrap();
    let request = Request::read(&mut reader, header.flexible).unwrap();
    let refused = decode_batches(request.topics[0].partitions[0].records.unwrap()).unwrap_err();
    assert!(
        matches!(refused, crate::Error::Checksum { .. }),
        "{refused}"
    );
}

#[test]
fn an_encoded_batch_decodes_to_what_was_written() {
    let headers = [("trace", Some(&b"abc"[..])), ("empty", None)];
    let records = [
        Outgoing {
            timestamp: 1_760_000_000_000,
            key: Some(b"k"),
            value: Some(b"first"),
            headers: &headers,
        },
        Outgoing {
            timestamp: 1_760_000_000_250,
            key: None,
            value: None,
            headers: &[],
        },
    ];
    let mut writer = Writer::new();
    encode_batch(&mut writer, 41, &records).unwrap();
    let bytes = writer.into_bytes();
    let batches = decode_batches(&bytes).unwrap();
    assert_eq!(batches.len(), 1);
    let batch = &batches[0];
    assert_eq!(
        (batch.base_offset, batch.last_offset_delta, batch.codec),
        (41, 1, Codec::None)
    );
    assert_eq!(batch.records[0].value.as_deref(), Some(&b"first"[..]));
    assert_eq!(
        batch.records[0].headers[1],
        Header {
            key: "empty".to_owned(),
            value: None
        }
    );
    assert_eq!(batch.records[1].timestamp, 1_760_000_000_250);
    assert_eq!(batch.records[1].value, None);
}

#[test]
fn a_compressed_batch_cannot_claim_more_than_the_ceiling() {
    // A zstd frame whose content is all zeros past the ceiling: a few hundred
    // bytes of input that would otherwise allocate gigabytes.
    let zeros = vec![0_u8; DECOMPRESSED_CEILING + 1];
    let compressed =
        ruzstd::encoding::compress_to_vec(&zeros[..], ruzstd::encoding::CompressionLevel::Fastest);
    let refused = super::compression::decompress(Codec::Zstd, &compressed).unwrap_err();
    assert!(
        matches!(refused, crate::Error::TooLarge { .. }),
        "{refused}"
    );
}
