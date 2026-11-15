//! The chunk footer's equality bloom filter (SG3 T3.1, the design record).
//!
//! A bloom answers membership with no false negatives and a declared
//! false-positive rate; at v1 it is sized for **1 % fpr at 10 bits per
//! non-null value** (the storage-internals canon: `bits ≈ -log2(fp) / ln 2`,
//! `k ≈ bits/n × ln 2 ≈ 7`). It prunes equality predicates only — a range
//! predicate can never consult it, and `!=` never prunes on it either (a
//! miss does not disprove inequality). An absent bloom disables pruning;
//! a wrong one would delete rows, which is why membership is computed over
//! the payload encoder's own bytes (the same normal form comparison uses —
//! equal values compare equal and encode equal).
//!
//! # The hash
//!
//! FNV-1a 64 double hashing (`idx_i = h1 + i × h2`, `h2` odd). The hash is a
//! local implementation detail: murmur2, the workspace's other hash, lives
//! in `tessari-session` above this layer and matters where Kafka-compatible
//! routing does — a bloom's distribution requirements are met by any
//! decent 64-bit hash (SG3 plan the design record).

use super::codec::Cursor;

/// Target false-positive rate at v1: 1 %, at 10 bits per inserted value.
const BITS_PER_VALUE: usize = 10;

/// The number of probe positions that rate sizes to (`10 × ln 2 ≈ 7`).
const HASH_COUNT: u8 = 7;

/// The smallest filter written, so even an all-null chunk's bloom occupies
/// real bytes and reads back the same shape.
const MIN_BIT_COUNT: usize = 64;

/// A hard sanity bound on a decoded filter's byte length, relative to what
/// one sealed chunk could have produced: a chunk seals at 256 KiB, and the
/// filter is 1.25 bytes per value at most, so anything past the chunk byte
/// ceiling is hostile regardless of the row count it claims.
const MAX_BLOOM_BYTES: usize = super::chunk::BYTES_PER_CHUNK;

/// A decoded bloom filter: the bit bytes, the bit count, and the probe count.
pub(crate) struct BloomBits {
    bytes: Vec<u8>,
    bit_count: usize,
    hash_count: u8,
}

impl BloomBits {
    /// Build a filter over the payload encodings of the non-null values.
    pub(crate) fn build(payloads: &[Vec<u8>]) -> Self {
        // The bit count is rounded up to a whole byte and stored implicitly
        // as the byte length: decode recomputes it as `byte_len × 8`, so the
        // two sides must agree exactly or every probe lands off its bit.
        let byte_count = payloads
            .len()
            .saturating_mul(BITS_PER_VALUE)
            .max(MIN_BIT_COUNT)
            .div_ceil(8);
        let mut bits = Self {
            bytes: vec![0_u8; byte_count],
            bit_count: byte_count.saturating_mul(8),
            hash_count: HASH_COUNT,
        };
        for payload in payloads {
            bits.insert(payload);
        }
        bits
    }

    /// Whether the payload's value may be in the set — always true when it
    /// is, sometimes true when it is not (the declared false-positive rate).
    /// Read by SG3's pruning scan.
    pub(crate) fn might_contain(&self, payload: &[u8]) -> bool {
        positions(fnv1a_64(payload), self.bit_count, self.hash_count).all(|at| {
            let byte = self.bytes.get(at >> 3).copied().unwrap_or(0);
            byte & (1_u8 << (at & 7)) != 0
        })
    }

    /// Append the encoded filter: `[byte_len:u32][bytes][hash_count:u8]`.
    pub(crate) fn encode_into(&self, out: &mut Vec<u8>) {
        super::chunk::put_u32(out, self.bytes.len());
        out.extend_from_slice(&self.bytes);
        out.push(self.hash_count);
    }

    /// Decode a filter from the footer cursor, refusing a length no sealed
    /// chunk could have written before any allocation sizes itself from it.
    pub(crate) fn decode(cursor: &mut Cursor<'_>) -> crate::error::Result<Self> {
        let byte_len = usize::try_from(cursor.u32()?).unwrap_or(usize::MAX);
        if byte_len == 0 || byte_len > MAX_BLOOM_BYTES {
            return Err(cursor.truncated());
        }
        let bytes = cursor.take(byte_len)?.to_vec();
        let hash_count = cursor.u8()?;
        if hash_count == 0 || hash_count > 32 {
            return Err(cursor.truncated());
        }
        let bit_count = byte_len.saturating_mul(8);
        Ok(Self {
            bytes,
            bit_count,
            hash_count,
        })
    }

    /// Insert one payload's worth of bits.
    fn insert(&mut self, payload: &[u8]) {
        for at in positions(fnv1a_64(payload), self.bit_count, self.hash_count) {
            if let Some(byte) = self.bytes.get_mut(at >> 3) {
                *byte |= 1_u8 << (at & 7);
            }
        }
    }
}

/// The probe positions for one hash: double hashing (`h1 + i × h2`) with the
/// stride forced odd so the walk never pins to a sublattice.
fn positions(hash: u64, bit_count: usize, hash_count: u8) -> impl Iterator<Item = usize> {
    let stride = (hash >> 32) | 1;
    let bit_count = u64::try_from(bit_count).unwrap_or(u64::MAX);
    let hash_count = u64::from(hash_count);
    (0..hash_count).map(move |round| {
        // The divisor is the bit count, which both constructors keep at one
        // or more (a minimum on build, a nonzero-byte refusal on decode), so
        // the checked form cannot fail; the fallback exists for the lint.
        let mixed = hash
            .wrapping_add(round.wrapping_mul(stride))
            .checked_rem(bit_count)
            .unwrap_or(0);
        usize::try_from(mixed).unwrap_or(usize::MAX)
    })
}

/// FNV-1a 64 — the local hash this filter double-probes with.
fn fnv1a_64(bytes: &[u8]) -> u64 {
    let mut hash = 0xcbf2_9ce4_8422_2325_u64;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3_u64);
    }
    hash
}
