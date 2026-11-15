//! The open-chunk writer: one per (table, family), holding the chunk appends
//! land in until it seals (worksheet §5.1, D-SG2-1).
//!
//! A writer is loaded from the committed state — the `'P'` pointer names its
//! open chunk seq, and that chunk's newest version decodes into the rows
//! already held — and flushed into the commit batch as sealed chunk
//! versions plus the moved pointer. Sealed chunks are immutable: a record's
//! value changing after its chunk sealed is a copy-on-write rewrite of that
//! chunk (§5.2), not a mutation of it.
//!
//! # The seal boundary (§4.1)
//!
//! A chunk seals at [`ROWS_PER_CHUNK`] rows or [`BYTES_PER_CHUNK`] bytes,
//! whichever comes first. Rows seal exactly at the boundary; a byte overflow
//! seals the largest prefix of rows that fits, so one enormous value cannot
//! pin an ever-growing chunk. The 8 192 / 256 KiB pair is the worksheet's
//! prediction, re-measured in C8 — the boundary lives here, in one place.

use std::collections::HashSet;

use tessari_types::{RecordId, Value};

use crate::error::Result;

use super::chunk::{BYTES_PER_CHUNK, ROWS_PER_CHUNK, decode_chunk, encode_chunk};

/// One chunk ready to write: its seq within the family and its bytes.
pub(crate) struct SealedChunk {
    /// The chunk's sequence within its family — append order.
    pub(crate) seq: u64,
    /// The encoded chunk.
    pub(crate) bytes: Vec<u8>,
}

/// The open chunk of one family, with the sealed chunks the appends produced.
pub(crate) struct ChunkWriter {
    seq: u64,
    rows: Vec<RecordId>,
    values: Vec<Value>,
    /// The identities holding rows — the same membership `rows` carries, kept
    /// as a set because an append asks "does this record hold a row?" once
    /// per record and a scan of `rows` is quadratic across a batch.
    held: HashSet<RecordId>,
    /// An upper estimate of the open chunk's encoded size, maintained
    /// incrementally so the byte boundary is examined without re-encoding
    /// the chunk on every append.
    approx: usize,
    sealed: Vec<SealedChunk>,
}

impl ChunkWriter {
    /// A writer at `seq` holding the rows of the chunk already there.
    pub(crate) fn loaded(seq: u64, rows: Vec<RecordId>, values: Vec<Value>) -> Self {
        let held = rows.iter().cloned().collect();
        let approx = rows
            .iter()
            .zip(values.iter())
            .map(|(id, value)| {
                id_len(id).saturating_add(tessari_encoding::encode_payload(value).len())
            })
            .sum();
        Self {
            seq,
            rows,
            values,
            held,
            approx,
            sealed: Vec::new(),
        }
    }

    /// Where a record's row lives in this writer — the open chunk.
    pub(crate) fn seq(&self) -> u64 {
        self.seq
    }

    /// Whether `id` already holds a row here.
    pub(crate) fn holds(&self, id: &RecordId) -> bool {
        self.held.contains(id)
    }

    /// Append a row. The record must not already hold one — an arriving
    /// record is new to this chunk, or the caller takes the upsert path.
    pub(crate) fn append(&mut self, id: RecordId, value: Value) {
        self.approx = self.approx.saturating_add(
            id_len(&id).saturating_add(tessari_encoding::encode_payload(&value).len()),
        );
        self.rows.push(id.clone());
        self.values.push(value);
        self.held.insert(id);
        self.seal_where_needed();
    }

    /// Replace a row the writer already holds.
    pub(crate) fn replace(&mut self, id: &RecordId, value: Value) {
        if let Some(at) = self.rows.iter().position(|held| held == id)
            && let Some(slot) = self.values.get_mut(at)
        {
            *slot = value;
        }
    }

    /// Re-estimate the open chunk's encoded size from its rows.
    fn reestimate(&mut self) {
        self.approx = self
            .rows
            .iter()
            .zip(self.values.iter())
            .map(|(id, value)| {
                id_len(id).saturating_add(tessari_encoding::encode_payload(value).len())
            })
            .sum();
    }

    /// The sealed chunks and the open chunk's final state.
    pub(crate) fn finish(self) -> Vec<SealedChunk> {
        let mut out = self.sealed;
        out.push(SealedChunk {
            seq: self.seq,
            bytes: encode_chunk(&self.rows, &self.values),
        });
        out
    }

    /// Seal while a boundary says the open chunk is full.
    fn seal_where_needed(&mut self) {
        while self.rows.len() > ROWS_PER_CHUNK {
            self.seal_prefix(ROWS_PER_CHUNK);
        }
        // Byte boundary, examined only when the estimate crosses: the common
        // case seals at the row boundary long before bytes bind, and checking
        // without the estimate re-encodes the chunk on every append. When it
        // does cross, seal the largest prefix that fits, so a chunk of few
        // enormous values still seals rather than growing without one.
        if self.approx > BYTES_PER_CHUNK && self.rows.len() > 1 {
            loop {
                let encoded = encode_chunk(&self.rows, &self.values);
                if encoded.len() <= BYTES_PER_CHUNK || self.rows.len() <= 1 {
                    break;
                }
                let mut prefix = self.rows.len() / 2;
                while prefix > 1
                    && encode_chunk(&self.rows[..prefix], &self.values[..prefix]).len()
                        > BYTES_PER_CHUNK
                {
                    prefix /= 2;
                }
                self.seal_prefix(prefix.max(1));
                self.reestimate();
            }
        }
    }

    /// Move the first `count` rows into a sealed chunk and open the next seq.
    fn seal_prefix(&mut self, count: usize) {
        let rest_rows = self.rows.split_off(count);
        let rest_values = self.values.split_off(count);
        let sealed_ids: HashSet<RecordId> = self.rows.iter().cloned().collect();
        let bytes = encode_chunk(&self.rows, &self.values);
        self.sealed.push(SealedChunk {
            seq: self.seq,
            bytes,
        });
        self.seq = self.seq.saturating_add(1);
        self.rows = rest_rows;
        self.values = rest_values;
        // The sealed rows' membership left the open chunk; the remainder's
        // membership is whatever the sealed set did not take.
        self.held = self.held.difference(&sealed_ids).cloned().collect();
        self.reestimate();
    }
}

/// What one record id contributes to the chunk's record-id index: its length
/// prefix plus its encoded form.
fn id_len(id: &RecordId) -> usize {
    4_usize.saturating_add(tessari_encoding::encode_record_id(id).len())
}

/// Remove a record's row from an encoded (sealed) chunk, for the
/// copy-on-write an update makes of a chunk it can no longer touch (§5.2).
///
/// `None` when the record holds no row there — a stale map entry is a state
/// to tolerate, not an error, because the row authority decides what is
/// true and the map is only a pointer.
pub(crate) fn remove_row(bytes: &[u8], id: &RecordId) -> Result<Option<Vec<u8>>> {
    let mut chunk = decode_chunk(bytes)?;
    let Some(at) = chunk.record_ids.iter().position(|held| held == id) else {
        return Ok(None);
    };
    chunk.record_ids.remove(at);
    chunk.values.remove(at);
    chunk.null_count = chunk
        .values
        .iter()
        .filter(|value| matches!(value, Value::Null | Value::None))
        .count();
    Ok(Some(encode_chunk(&chunk.record_ids, &chunk.values)))
}
