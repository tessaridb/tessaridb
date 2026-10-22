//! The record batch, magic 2: what a producer sends and a fetch answers.
//!
//! A batch is a header the whole batch shares (base offset, timestamps, the
//! producer's identity and sequence) and a list of records stored as deltas
//! from it. The CRC-32C covers everything from the attributes to the end, so a
//! batch is checked once, before anything in it is believed.

use crate::codec::{Reader, Writer};
use crate::error::{Error, Result};
use crate::records::compression::{Codec, decompress};

/// The only batch format this node reads or writes.
const MAGIC: i8 = 2;

/// Bytes from the start of a batch to the end of its `batchLength` field.
const LOG_OVERHEAD: usize = 12;

/// Bytes from the start of the batch to the attributes, which is where the
/// checksum starts.
const CRC_START: usize = 21;

/// The attribute bits that are not the codec.
const TIMESTAMP_TYPE: i16 = 0x08;
const TRANSACTIONAL: i16 = 0x10;
const CONTROL: i16 = 0x20;

/// One header of a record.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Header {
    /// Its name.
    pub key: String,
    /// Its value, which the protocol allows to be null.
    pub value: Option<Vec<u8>>,
}

/// One record, with its batch's deltas resolved.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Record {
    /// Its offset within the batch.
    pub offset_delta: i32,
    /// Its timestamp in milliseconds since the epoch.
    pub timestamp: i64,
    /// Its key, if it has one.
    pub key: Option<Vec<u8>>,
    /// Its value; null is a tombstone in the protocol's terms.
    pub value: Option<Vec<u8>>,
    /// Its headers, in the order the producer wrote them.
    pub headers: Vec<Header>,
}

/// A decoded batch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Batch {
    /// The offset the producer wrote (zero from a producer; the log's on fetch).
    pub base_offset: i64,
    /// The codec its records were compressed with.
    pub codec: Codec,
    /// Whether the timestamps are the log's rather than the producer's.
    pub log_append_time: bool,
    /// The producer's id, −1 for a producer that is not idempotent.
    pub producer_id: i64,
    /// The producer's epoch.
    pub producer_epoch: i16,
    /// The first record's sequence, −1 for a producer that is not idempotent.
    pub base_sequence: i32,
    /// The highest offset delta, which says how many offsets the batch takes.
    pub last_offset_delta: i32,
    /// The records.
    pub records: Vec<Record>,
}

/// Every batch in a produce request's `records` field.
///
/// # Errors
///
/// [`Error::Malformed`] for a batch that is not magic 2, is transactional or a
/// control batch, or whose record count disagrees with its records;
/// [`Error::Checksum`] for one whose bytes do not match its CRC; anything the
/// codec refuses.
pub fn decode_batches(bytes: &[u8]) -> Result<Vec<Batch>> {
    let mut batches = Vec::new();
    let mut reader = Reader::new(bytes);
    while reader.remaining() > 0 {
        let mut head = reader.clone();
        let _base = head.i64("baseOffset")?;
        let length = usize::try_from(head.i32("batchLength")?).map_err(|_| Error::Malformed {
            field: "batchLength",
            why: "negative",
        })?;
        let total = LOG_OVERHEAD
            .checked_add(length)
            .ok_or(Error::Truncated { field: "batch" })?;
        let whole = reader.take(total, "batch")?;
        batches.push(decode_one(whole)?);
    }
    Ok(batches)
}

fn decode_one(whole: &[u8]) -> Result<Batch> {
    let mut reader = Reader::new(whole);
    let base_offset = reader.i64("baseOffset")?;
    let _length = reader.i32("batchLength")?;
    let _leader_epoch = reader.i32("partitionLeaderEpoch")?;
    let magic = reader.i8("magic")?;
    if magic != MAGIC {
        return Err(Error::Malformed {
            field: "magic",
            why: "only record batch format 2 is read (produce from version 3)",
        });
    }
    let stored = reader.u32("crc")?;
    let covered = whole
        .get(CRC_START..)
        .ok_or(Error::Truncated { field: "crc" })?;
    let computed = crc32c::crc32c(covered);
    if stored != computed {
        return Err(Error::Checksum { stored, computed });
    }
    let attributes = reader.i16("attributes")?;
    if attributes & (TRANSACTIONAL | CONTROL) != 0 {
        return Err(Error::Malformed {
            field: "attributes",
            why: "a transactional or control batch; transactions are not served",
        });
    }
    let codec = Codec::of(attributes)?;
    let last_offset_delta = reader.i32("lastOffsetDelta")?;
    let base_timestamp = reader.i64("baseTimestamp")?;
    let _max_timestamp = reader.i64("maxTimestamp")?;
    let producer_id = reader.i64("producerId")?;
    let producer_epoch = reader.i16("producerEpoch")?;
    let base_sequence = reader.i32("baseSequence")?;
    let count = reader.i32("recordsCount")?;
    let count = usize::try_from(count).map_err(|_| Error::Malformed {
        field: "recordsCount",
        why: "negative",
    })?;
    let body = decompress(codec, reader.rest())?;
    let mut records_reader = Reader::new(&body);
    let mut records = Vec::with_capacity(count.min(records_reader.remaining()));
    for _ in 0..count {
        records.push(decode_record(&mut records_reader, base_timestamp)?);
    }
    if records_reader.remaining() != 0 {
        return Err(Error::Malformed {
            field: "records",
            why: "bytes after the last record the count names",
        });
    }
    Ok(Batch {
        base_offset,
        codec,
        log_append_time: attributes & TIMESTAMP_TYPE != 0,
        producer_id,
        producer_epoch,
        base_sequence,
        last_offset_delta,
        records,
    })
}

fn varint_len(reader: &mut Reader<'_>, field: &'static str) -> Result<Option<usize>> {
    let length = reader.varint(field)?;
    if length < 0 {
        return Ok(None);
    }
    usize::try_from(length)
        .map(Some)
        .map_err(|_| Error::Malformed {
            field,
            why: "a length this platform cannot hold",
        })
}

fn decode_record(reader: &mut Reader<'_>, base_timestamp: i64) -> Result<Record> {
    let length = varint_len(reader, "record length")?.ok_or(Error::Malformed {
        field: "record length",
        why: "negative",
    })?;
    let mut record = Reader::new(reader.take(length, "record")?);
    let _attributes = record.i8("record attributes")?;
    let timestamp_delta = record.varlong("timestampDelta")?;
    let offset_delta = record.varint("offsetDelta")?;
    let key = match varint_len(&mut record, "key length")? {
        Some(length) => Some(record.take(length, "key")?.to_vec()),
        None => None,
    };
    let value = match varint_len(&mut record, "value length")? {
        Some(length) => Some(record.take(length, "value")?.to_vec()),
        None => None,
    };
    let header_count = varint_len(&mut record, "header count")?.unwrap_or(0);
    let mut headers = Vec::with_capacity(header_count.min(record.remaining()));
    for _ in 0..header_count {
        let key_length = varint_len(&mut record, "header key length")?.ok_or(Error::Malformed {
            field: "header key",
            why: "null",
        })?;
        let key = std::str::from_utf8(record.take(key_length, "header key")?)
            .map_err(|_| Error::Malformed {
                field: "header key",
                why: "not UTF-8",
            })?
            .to_owned();
        let value = match varint_len(&mut record, "header value length")? {
            Some(length) => Some(record.take(length, "header value")?.to_vec()),
            None => None,
        };
        headers.push(Header { key, value });
    }
    if record.remaining() != 0 {
        return Err(Error::Malformed {
            field: "record",
            why: "bytes after its last header",
        });
    }
    Ok(Record {
        offset_delta,
        timestamp: base_timestamp
            .checked_add(timestamp_delta)
            .ok_or(Error::Malformed {
                field: "timestampDelta",
                why: "overflows the base timestamp",
            })?,
        key,
        value,
        headers,
    })
}

/// One header of an outgoing record: its name and its value.
pub type OutgoingHeader<'a> = (&'a str, Option<&'a [u8]>);

/// A record to put in a fetched batch.
#[derive(Debug, Clone, Copy)]
pub struct Outgoing<'a> {
    /// Its timestamp in milliseconds since the epoch.
    pub timestamp: i64,
    /// Its key.
    pub key: Option<&'a [u8]>,
    /// Its value.
    pub value: Option<&'a [u8]>,
    /// Its headers.
    pub headers: &'a [OutgoingHeader<'a>],
}

fn put_varint_len(writer: &mut Writer, length: Option<usize>) -> Result<()> {
    let encoded = match length {
        None => -1,
        Some(length) => i32::try_from(length).map_err(|_| Error::TooLarge {
            field: "record field",
            found: u64::try_from(length).unwrap_or(u64::MAX),
            ceiling: u64::from(i32::MAX.unsigned_abs()),
        })?,
    };
    writer.varint(encoded);
    Ok(())
}

/// One uncompressed batch of consecutive offsets starting at `base_offset`.
///
/// # Errors
///
/// [`Error::TooLarge`] for a field or a batch past what the format can carry.
pub fn encode_batch(writer: &mut Writer, base_offset: i64, records: &[Outgoing<'_>]) -> Result<()> {
    let too_many = || Error::TooLarge {
        field: "records",
        found: u64::try_from(records.len()).unwrap_or(u64::MAX),
        ceiling: u64::from(i32::MAX.unsigned_abs()),
    };
    let count = i32::try_from(records.len()).map_err(|_| too_many())?;
    let base_timestamp = records.first().map_or(-1, |record| record.timestamp);
    let max_timestamp = records
        .iter()
        .map(|record| record.timestamp)
        .max()
        .unwrap_or(-1);
    let start = writer.len();
    writer.i64(base_offset);
    let length_at = writer.len();
    writer.i32(0);
    writer.i32(-1);
    writer.i8(MAGIC);
    let crc_at = writer.len();
    writer.u32(0);
    let covered_from = writer.len();
    writer.i16(0);
    writer.i32(count.checked_sub(1).unwrap_or(0).max(0));
    writer.i64(base_timestamp);
    writer.i64(max_timestamp);
    writer.i64(-1);
    writer.i16(-1);
    writer.i32(-1);
    writer.i32(count);
    for (index, record) in records.iter().enumerate() {
        let mut body = Writer::new();
        body.i8(0);
        body.varlong(
            record
                .timestamp
                .checked_sub(base_timestamp)
                .ok_or(Error::Malformed {
                    field: "timestamp",
                    why: "too far from the batch's first",
                })?,
        );
        body.varint(i32::try_from(index).map_err(|_| too_many())?);
        put_varint_len(&mut body, record.key.map(<[u8]>::len))?;
        if let Some(key) = record.key {
            body.raw(key);
        }
        put_varint_len(&mut body, record.value.map(<[u8]>::len))?;
        if let Some(value) = record.value {
            body.raw(value);
        }
        put_varint_len(&mut body, Some(record.headers.len()))?;
        for (key, value) in record.headers {
            put_varint_len(&mut body, Some(key.len()))?;
            body.raw(key.as_bytes());
            put_varint_len(&mut body, value.map(<[u8]>::len))?;
            if let Some(value) = value {
                body.raw(value);
            }
        }
        put_varint_len(writer, Some(body.len()))?;
        writer.raw(&body.into_bytes());
    }
    let crc = crc32c::crc32c(writer.since(covered_from));
    writer.patch_u32(crc_at, crc)?;
    let length = writer
        .len()
        .checked_sub(start)
        .and_then(|whole| whole.checked_sub(LOG_OVERHEAD))
        .and_then(|length| u32::try_from(length).ok())
        .ok_or(Error::TooLarge {
            field: "batch",
            found: u64::try_from(writer.len()).unwrap_or(u64::MAX),
            ceiling: u64::from(i32::MAX.unsigned_abs()),
        })?;
    writer.patch_u32(length_at, length)
}
