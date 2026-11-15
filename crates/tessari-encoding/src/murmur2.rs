//! The 32-bit murmur2 hash the Kafka clients' default partitioner applies to
//! a record's key — the hash `PARTITIONS n BY` routes by, on topics since
//! topics and, on columnar tables, the storage layer's projection
//! and the session's write funnel must hash identically, which is why this
//! lives in the encoding crate both consume rather than beside either).
//!
//! Seed `0x9747b28c`, four bytes at a time little-endian, the tail folded in
//! from its last byte down. The topic partition tests hold the known
//! vectors .

/// The murmur2 hash of `data`, as the reference partitioner computes it.
#[must_use]
pub fn murmur2(data: &[u8]) -> u32 {
    const M: u32 = 0x5bd1_e995;
    const R: u32 = 24;
    let length = u32::try_from(data.len()).unwrap_or(u32::MAX);
    let mut hash = 0x9747_b28c ^ length;
    let (chunks, tail) = data.as_chunks::<4>();
    for chunk in chunks {
        let mut k = u32::from_le_bytes(*chunk);
        k = k.wrapping_mul(M);
        k ^= k >> R;
        k = k.wrapping_mul(M);
        hash = hash.wrapping_mul(M) ^ k;
    }
    let folded = match *tail {
        [first, second, third] => {
            Some(u32::from(third) << 16 | u32::from(second) << 8 | u32::from(first))
        }
        [first, second] => Some(u32::from(second) << 8 | u32::from(first)),
        [first] => Some(u32::from(first)),
        _ => None,
    };
    if let Some(folded) = folded {
        hash = (hash ^ folded).wrapping_mul(M);
    }
    hash ^= hash >> 13;
    hash = hash.wrapping_mul(M);
    hash ^ (hash >> 15)
}

#[cfg(test)]
mod tests {
    use super::murmur2;

    /// The vectors the Kafka clients' own partitioner answers (kafka-python
    /// `murmur2`, unsigned; "21" and "foobar" are its published test vectors)
    /// — carried with the hash so both consumers test against the same table.
    #[test]
    fn the_hash_is_the_one_kafka_clients_choose_a_partition_by() {
        for (key, expected) in [
            ("", 275_646_681_u32),
            ("21", 3_321_034_988),
            ("abc", 479_470_107),
            ("foobar", 3_504_634_814),
            ("a-little-bit-long-string", 3_308_985_760),
            ("user-42", 1_459_644_460),
            ("ключ", 2_122_343_024_u32),
        ] {
            assert_eq!(murmur2(key.as_bytes()), expected, "{key:?}");
        }
    }
}
