//! A column chunk: the unit one field's projection is written, read and
//! copy-on-write rewritten in (worksheet §4).
//!
//! # The payload layout
//!
//! ```text
//! [count:u32]            rows in the chunk
//! [encoding:u8]          the data section's encoding tag (codec.rs)
//! [flags:u8]             0x01: the non-null values are non-decreasing
//! [null_count:u32]       rows whose slot is null
//! [validity bitmap]      (count + 7) / 8 bytes, iff null_count > 0; a set
//!                        bit marks a null slot
//! [data section]         the non-null values in row order, encoded
//! [record ids]           u32 byte length, then per row: u32 length + the
//!                        record id, preserving the append order
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
//! # Statistics are written by the writer, and absent is honest (§4.3)
//!
//! The format reserves flag bits for the footer statistics (min/max, a
//! bloom filter), and at this wave they are always absent: an absent
//! statistic disables pruning, a wrong one deletes rows. SG3 (T3.1) fills
//! them. `sorted` and `null_count` are written from the start — they fall
//! out of encoding and cost nothing to keep honest.
//!
//! # The record-id index
//!
//! The writer knows which record each row belongs to; the index preserves
//! that mapping so a decoded run can be zipped back to identities without a
//! second structure (§4.3). Identities are stored per row — sequential
//! integer ids compress later (C8 measures), and exactness is not traded
//! for the ratio at v1.

use tessari_types::{RecordId, Value};

use crate::error::Result;

use super::codec::{self, Cursor};

/// Rows a chunk seals at — the §4.1 prediction, re-measured in C8.
pub(crate) const ROWS_PER_CHUNK: usize = 8192;

/// Bytes a chunk seals at, whichever comes first — the §4.1 prediction.
pub(crate) const BYTES_PER_CHUNK: usize = 256 * 1024;

/// A hard ceiling on a header's row count, refusing absurd counts before any
/// allocation sizes itself from them. Generous on purpose: chunks seal at
/// [`ROWS_PER_CHUNK`] today, and a future build may raise the seal — this
/// bound only stops a hostile header from turning one into an allocation.
const ROW_CEILING: usize = 1 << 24;

/// A decoded column chunk: identities, values, and what the header recorded.
pub(crate) struct ColumnChunk {
    /// The record id of each row, in append order.
    pub(crate) record_ids: Vec<RecordId>,
    /// The value of each row; a null slot is [`Value::Null`].
    pub(crate) values: Vec<Value>,
    /// Whether the header recorded the non-null values as non-decreasing.
    /// Read by the chunk tests today and by SG3's pruning (a sorted chunk
    /// takes the binary-search path); kept on the decoded form because it is
    /// part of what the header says.
    #[allow(dead_code)]
    pub(crate) sorted: bool,
    /// Rows whose slot is null.
    pub(crate) null_count: usize,
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

    let mut out = Vec::new();
    put_u32(&mut out, count);
    out.push(encoding.tag());
    let sorted = non_null.windows(2).all(|pair| pair[0] <= pair[1]);
    out.push(u8::from(sorted));
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
    cursor.finish()?;

    Ok(ColumnChunk {
        record_ids,
        values,
        sorted: flags & 0x01 != 0,
        null_count,
    })
}

fn put_u32(out: &mut Vec<u8>, value: usize) {
    // Rows never exceed a sealed chunk's cap (ROWS_PER_CHUNK), so the
    // conversion cannot fail; the saturation idiom is the codebase's own
    // (tessari-encoding scalars), keeping the bytes writable rather than
    // panicking on a bug.
    out.extend_from_slice(&u32::try_from(value).unwrap_or(u32::MAX).to_be_bytes());
}
