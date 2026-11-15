//! A column chunk: the unit one field's projection is written, read and
//! copy-on-write rewritten in (worksheet §4).
//!
//! # The payload layout
//!
//! ```text
//! [count:u32]            rows in the chunk
//! [encoding:u8]          the data section's encoding tag (codec.rs)
//! [flags:u8]             0x01: the non-null values are non-decreasing
//!                        0x02: min/max statistics are present
//!                        0x04: a bloom filter is present
//!                        0x08: range statistics are disabled (a NaN sat
//!                              the column — worksheet §4.3)
//! [null_count:u32]       rows whose slot is null
//! [validity bitmap]      (count + 7) / 8 bytes, iff null_count > 0; a set
//!                        bit marks a null slot
//! [data section]         the non-null values in row order, encoded
//! [record ids]           u32 byte length, then per row: u32 length + the
//!                        record id, preserving the append order
//! [stats footer]         iff 0x02: min payload, max payload (each u32
//!                        length + encoded value, the store's own
//!                        comparator); iff 0x04: the bloom filter
//!                        (bloom.rs)
//! ```
//!
//! # Nulls are a bitmap, never a sentinel (§4.3)
//!
//! A null slot is never a reserved value the data section could collide
//! with. `null_count` is load-bearing: a zero-null chunk takes the
//! branch-free decode path (no bitmap at all), an all-null chunk has an
//! empty data section, and a decoded chunk whose bitmap and `null_count`
//! disagree is refused.
//!
//! # Statistics are written by the writer, and absent is honest (§4.3/§4.4)
//!
//! min/max and the bloom are computed by `encode_chunk` — the one writer —
//! so every path that lands a chunk (the open-chunk flush, the seal, the
//! copy-on-write a sealed chunk owes an update) writes them in the same
//! batch as the data, and no statistics-maintenance job exists to fall
//! behind. A statistic that cannot be computed exactly is absent: an
//! all-null column has nothing to order, and a float NaN disables range
//! statistics for its chunk. An absent statistic disables pruning; a wrong
//! one would delete rows.
//!
//! # The record-id index
//!
//! The writer knows which record each row belongs to; the index preserves
//! that mapping so a decoded run can be zipped back to identities without a
//! second structure (§4.3). Identities are stored per row — sequential
//! integer ids compress later (C8 measures), and exactness is not traded
//! for the ratio at v1.

use tessari_encoding::{decode_payload, encode_payload};
use tessari_types::{RecordId, Value};

use crate::error::Result;

use super::bloom::BloomBits;
use super::codec::{self, Cursor};

/// Rows a chunk seals at — the §4.1 prediction, re-measured in C8.
pub(crate) const ROWS_PER_CHUNK: usize = 8192;

/// Bytes a chunk seals at, whichever comes first — the §4.1 prediction.
pub(crate) const BYTES_PER_CHUNK: usize = 256 * 1024;

/// `flags`: the non-null values are non-decreasing.
const FLAG_SORTED: u8 = 0x01;

/// `flags`: min/max statistics are present in the footer.
const FLAG_MINMAX: u8 = 0x02;

/// `flags`: a bloom filter is present in the footer.
const FLAG_BLOOM: u8 = 0x04;

/// `flags`: a NaN sat the column; range statistics are disabled for this
/// chunk even though other footer fields may be present.
const FLAG_RANGE_DISABLED: u8 = 0x08;

/// A hard ceiling on a header's row count, refusing absurd counts before any
/// allocation sizes itself from them. Generous on purpose: chunks seal at
/// [`ROWS_PER_CHUNK`] today, and a future build may raise the seal — this
/// bound only stops a hostile header from turning one into an allocation.
const ROW_CEILING: usize = 1 << 24;

/// The footer statistics one chunk carries (SG3 T3.1).
///
/// Every member is `Option` because absence is a first-class state the scan
/// consults: an absent statistic disables the pruning it would serve, which
/// is cheaper than a wrong one (§4.3). `range_disabled` records *why* a
/// chunk with values carries no min/max — a NaN sat the column — so the
/// scan's report can say it instead of guessing.
///
/// The members are read by SG3's pruning scan (`columnar/scan.rs`, landing
/// later in this wave); the expectations below are removed there.
#[allow(dead_code)]
pub(crate) struct ChunkStats {
    /// The least non-null value under the store's one declared order.
    pub(crate) min: Option<Value>,
    /// The greatest non-null value under the same order.
    pub(crate) max: Option<Value>,
    /// The equality bloom over the non-null values' payload encodings.
    pub(crate) bloom: Option<BloomBits>,
    /// Whether a NaN disabled range statistics for this chunk.
    pub(crate) range_disabled: bool,
}

/// A decoded column chunk: identities, values, and what the header recorded.
pub(crate) struct ColumnChunk {
    /// The record id of each row, in append order.
    pub(crate) record_ids: Vec<RecordId>,
    /// The value of each row; a null slot is [`Value::Null`].
    pub(crate) values: Vec<Value>,
    /// Whether the header recorded the non-null values as non-decreasing.
    /// Read by the chunk tests and by SG3's pruning (a sorted chunk takes
    /// the binary-search path); kept on the decoded form because it is part
    /// of what the header says.
    #[allow(dead_code)]
    pub(crate) sorted: bool,
    /// Rows whose slot is null.
    pub(crate) null_count: usize,
    /// The footer statistics the writer computed with the chunk — read by
    /// SG3's pruning scan, landing later in this wave.
    #[allow(dead_code)]
    pub(crate) stats: ChunkStats,
}

/// Whether a slot is null.
///
/// Both of the store's absences map here: `Null` (the field holds nothing)
/// and `None` (a schemaless record that does not mention the field — D-SG2-2
/// projects every field the table has seen, so the projection covers the
/// record with a null slot). Decoded chunks answer `Null` for both, and the
/// row path is the authority on the distinction.
fn is_null(value: &Value) -> bool {
    matches!(value, Value::Null | Value::None)
}

/// Encode one chunk: the record ids of its rows and their values.
///
/// `values.len()` must equal `record_ids.len()` — the caller building the
/// batch holds both, and the debug assertion is the whole contract; a
/// mismatch is a bug at the call site, not a state to recover from.
#[must_use]
pub(crate) fn encode_chunk(record_ids: &[RecordId], values: &[Value]) -> Vec<u8> {
    debug_assert_eq!(
        record_ids.len(),
        values.len(),
        "a chunk's rows and values disagree"
    );
    let count = record_ids.len().min(values.len());

    let null_count = values[..count]
        .iter()
        .filter(|value| is_null(value))
        .count();
    let non_null: Vec<Value> = values[..count]
        .iter()
        .filter(|value| !is_null(value))
        .cloned()
        .collect();
    let (encoding, data) = codec::choose(&non_null);

    // Footer statistics (§4.4): computed here, by the one writer, in the
    // same bytes as the data. A NaN anywhere in the column disables range
    // statistics for the chunk — min/max under a total order that has to
    // place NaN somewhere would be exactly the "wrong statistic" §4.3
    // refuses to write. The bloom is built over the payload encoder's own
    // bytes: the normal form comparison reduces to, so equal values encode
    // equal and the filter never denies a value the chunk holds.
    let range_disabled = non_null.iter().any(|value| match value {
        Value::Number(number) => number.is_nan(),
        _ => false,
    });
    let (min, max) = if range_disabled || non_null.is_empty() {
        (None, None)
    } else {
        (
            non_null.iter().min().cloned(),
            non_null.iter().max().cloned(),
        )
    };
    let bloom_payloads: Vec<Vec<u8>> = non_null
        .iter()
        .map(|value| encode_payload(value).into_bytes())
        .collect();
    let bloom = BloomBits::build(&bloom_payloads);

    let mut out = Vec::new();
    put_u32(&mut out, count);
    out.push(encoding.tag());
    let sorted = non_null.windows(2).all(|pair| pair[0] <= pair[1]);
    let mut flags = u8::from(sorted);
    if min.is_some() {
        flags |= FLAG_MINMAX;
    }
    flags |= FLAG_BLOOM;
    if range_disabled {
        flags |= FLAG_RANGE_DISABLED;
    }
    out.push(flags);
    put_u32(&mut out, null_count);

    if null_count > 0 {
        let mut bitmap = vec![0_u8; count.div_ceil(8)];
        for (row, value) in values[..count].iter().enumerate() {
            if is_null(value) {
                let byte = row >> 3;
                let bit = u8::try_from(row & 7).unwrap_or(0);
                if let Some(slot) = bitmap.get_mut(byte) {
                    *slot |= 1_u8 << bit;
                }
            }
        }
        out.extend_from_slice(&bitmap);
    }

    out.extend_from_slice(&data);

    let mut ids = Vec::new();
    for id in &record_ids[..count] {
        let encoded = tessari_encoding::encode_record_id(id);
        put_u32(&mut ids, encoded.len());
        ids.extend_from_slice(&encoded);
    }
    put_u32(&mut out, ids.len());
    out.extend_from_slice(&ids);

    // The stats footer: min then max (each a length-prefixed payload), then
    // the bloom. Absent min/max (all-null, or NaN-disabled) write nothing —
    // the flags byte is the authority for what follows.
    if let (Some(min), Some(max)) = (&min, &max) {
        let encoded = encode_payload(min).into_bytes();
        put_u32(&mut out, encoded.len());
        out.extend_from_slice(&encoded);
        let encoded = encode_payload(max).into_bytes();
        put_u32(&mut out, encoded.len());
        out.extend_from_slice(&encoded);
    }
    bloom.encode_into(&mut out);
    out
}

/// Decode one chunk, refusing anything that is not the shape it claims.
pub(crate) fn decode_chunk(bytes: &[u8]) -> Result<ColumnChunk> {
    let mut cursor = Cursor::new(bytes);
    let count = usize::try_from(cursor.u32()?).unwrap_or(usize::MAX);
    if count > ROW_CEILING {
        return Err(cursor.truncated());
    }
    let encoding = cursor.u8()?;
    let flags = cursor.u8()?;
    let null_count = usize::try_from(cursor.u32()?).unwrap_or(usize::MAX);
    if null_count > count {
        return Err(cursor.truncated());
    }
    let non_null_count = count
        .checked_sub(null_count)
        .ok_or_else(|| cursor.truncated())?;

    let mut null_slots = vec![false; count];
    if null_count > 0 {
        let bitmap = cursor.take(count.div_ceil(8))?;
        let mut counted = 0_usize;
        for (row, slot) in null_slots.iter_mut().enumerate() {
            // `>> 3` and `& 7` are `div 8` and `mod 8` for the non-negative
            // row index, without the arithmetic lint's reach.
            let byte = bitmap.get(row >> 3).copied().unwrap_or(0);
            let bit = u8::try_from(row & 7).unwrap_or(0);
            if byte & (1_u8 << bit) != 0 {
                *slot = true;
                counted = counted.saturating_add(1);
            }
        }
        // The bitmap is the authority for the slots; the count must agree.
        if counted != null_count {
            return Err(cursor.truncated());
        }
    }

    let decoded = codec::decode(encoding, &mut cursor, non_null_count)?;
    let mut next = decoded.into_iter();
    let mut values = Vec::with_capacity(count.min(1024));
    for slot in null_slots {
        if slot {
            values.push(Value::Null);
        } else {
            values.push(next.next().ok_or_else(|| cursor.truncated())?);
        }
    }

    // The record-id index: one id per row, in append order.
    let ids_len = usize::try_from(cursor.u32()?).unwrap_or(usize::MAX);
    let mut id_cursor = Cursor::new(cursor.take(ids_len)?);
    let mut record_ids = Vec::with_capacity(count.min(1024));
    for _ in 0..count {
        let len = usize::try_from(id_cursor.u32()?).unwrap_or(usize::MAX);
        let raw = id_cursor.take(len)?;
        record_ids.push(tessari_encoding::decode_record_id(raw)?);
    }
    id_cursor.finish()?;

    // The stats footer: min then max (each a length-prefixed payload), then
    // the bloom. A member the flags do not promise, or one that truncates,
    // refuses rather than guesses — a statistic read past its bytes would be
    // a wrong one, and a wrong one deletes rows.
    let minmax = if flags & FLAG_MINMAX != 0 {
        let min = decode_stat(&mut cursor)?;
        let max = decode_stat(&mut cursor)?;
        Some((min, max))
    } else {
        None
    };
    let bloom = if flags & FLAG_BLOOM != 0 {
        Some(BloomBits::decode(&mut cursor)?)
    } else {
        None
    };
    cursor.finish()?;

    Ok(ColumnChunk {
        record_ids,
        values,
        sorted: flags & FLAG_SORTED != 0,
        null_count,
        stats: ChunkStats {
            min: minmax.as_ref().map(|(min, _)| min.clone()),
            max: minmax.map(|(_, max)| max),
            bloom,
            range_disabled: flags & FLAG_RANGE_DISABLED != 0,
        },
    })
}

/// One footer statistic: a length-prefixed payload decoded back to its value.
fn decode_stat(cursor: &mut Cursor<'_>) -> Result<Value> {
    let len = usize::try_from(cursor.u32()?).unwrap_or(usize::MAX);
    let raw = cursor.take(len)?;
    decode_payload(raw).map_err(crate::Error::from)
}

pub(crate) fn put_u32(out: &mut Vec<u8>, value: usize) {
    // Rows never exceed a sealed chunk's cap (ROWS_PER_CHUNK), so the
    // conversion cannot fail; the saturation idiom is the codebase's own
    // (tessari-encoding scalars), keeping the bytes writable rather than
    // panicking on a bug.
    out.extend_from_slice(&u32::try_from(value).unwrap_or(u32::MAX).to_be_bytes());
}
