//! SHA-1, for one purpose only.
//!
//! RFC 6455's opening handshake proves the server understood the request by
//! hashing the client's key together with a fixed, published GUID. **This is not
//! a security primitive here and the RFC says so** — the input is public, the
//! GUID is a constant printed in the specification, and the answer is checked by
//! a client that already knows it. SHA-1's collision weakness is irrelevant to a
//! computation whose whole job is "you read my header".
//!
//! It is written out because at this size it is the smaller of the two options:
//! sixty lines against a crate whose own tree is four crates we do not otherwise
//! carry. Nothing else in this store hashes anything, so there is no second
//! caller waiting for a general implementation, and if one ever appears it will
//! want a real hash rather than this one.

/// The five words SHA-1 starts from — FIPS 180-4 §5.3.1.
const START: [u32; 5] = [
    0x6745_2301,
    0xefcd_ab89,
    0x98ba_dcfe,
    0x1032_5476,
    0xc3d2_e1f0,
];

/// The four round constants, one per twenty rounds.
const ROUND: [u32; 4] = [0x5a82_7999, 0x6ed9_eba1, 0x8f1b_bcdc, 0xca62_c1d6];

/// The SHA-1 digest of `message`.
pub(crate) fn digest(message: &[u8]) -> [u8; 20] {
    let mut state = START;
    let mut block = [0u8; 64];
    let mut chunks = message.chunks_exact(64);
    for chunk in chunks.by_ref() {
        block.copy_from_slice(chunk);
        compress(&mut state, &block);
    }

    // The tail: what is left, a single `0x80`, zeroes, and the message length in
    // **bits** as a big-endian u64. That length is where a hand-written SHA-1
    // usually goes wrong, so it is the thing the vector test is pointed at.
    let rest = chunks.remainder();
    // The fallback is unreachable where `usize` is 64 bits or fewer, which is
    // every platform this builds for; it is written rather than unwrapped
    // because the workspace refuses both a panic and a lossy cast.
    let bits = u64::try_from(message.len())
        .unwrap_or(u64::MAX)
        .wrapping_mul(8);
    block = [0; 64];
    block[..rest.len()].copy_from_slice(rest);
    block[rest.len()] = 0x80;
    if rest.len() >= 56 {
        // No room for the length in this block: finish it and use another.
        compress(&mut state, &block);
        block = [0; 64];
    }
    block[56..].copy_from_slice(&bits.to_be_bytes());
    compress(&mut state, &block);

    let mut out = [0u8; 20];
    for (word, slot) in state.iter().zip(out.chunks_exact_mut(4)) {
        slot.copy_from_slice(&word.to_be_bytes());
    }
    out
}

/// Fold one 64-byte block into the state.
fn compress(state: &mut [u32; 5], block: &[u8; 64]) {
    let mut schedule = [0u32; 80];
    for (word, bytes) in schedule.iter_mut().zip(block.chunks_exact(4)) {
        // Four bytes, and `chunks_exact(4)` promises exactly four.
        *word = u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]);
    }
    for index in 16..80usize {
        // The range is what makes these subtractions sound — `index` is never
        // below 16 — and the saturating form says so to the compiler as well as
        // to a reader.
        let mixed = schedule[index.saturating_sub(3)]
            ^ schedule[index.saturating_sub(8)]
            ^ schedule[index.saturating_sub(14)]
            ^ schedule[index.saturating_sub(16)];
        schedule[index] = mixed.rotate_left(1);
    }

    let [mut a, mut b, mut c, mut d, mut e] = *state;
    for (index, word) in schedule.iter().enumerate() {
        let (mix, constant) = match index {
            0..=19 => ((b & c) | (!b & d), ROUND[0]),
            20..=39 => (b ^ c ^ d, ROUND[1]),
            40..=59 => ((b & c) | (b & d) | (c & d), ROUND[2]),
            _ => (b ^ c ^ d, ROUND[3]),
        };
        // Wrapping is the specification's arithmetic, not an overflow being
        // tolerated: SHA-1 is defined modulo 2^32.
        let next = a
            .rotate_left(5)
            .wrapping_add(mix)
            .wrapping_add(e)
            .wrapping_add(constant)
            .wrapping_add(*word);
        e = d;
        d = c;
        c = b.rotate_left(30);
        b = a;
        a = next;
    }

    state[0] = state[0].wrapping_add(a);
    state[1] = state[1].wrapping_add(b);
    state[2] = state[2].wrapping_add(c);
    state[3] = state[3].wrapping_add(d);
    state[4] = state[4].wrapping_add(e);
}

#[cfg(test)]
mod tests {
    use super::digest;

    fn hex(bytes: [u8; 20]) -> String {
        bytes.iter().map(|byte| format!("{byte:02x}")).collect()
    }

    #[test]
    fn the_published_vectors_are_reproduced() {
        // FIPS 180-4's own examples, plus the empty input, which is the case a
        // length-padding mistake gets wrong first.
        assert_eq!(
            hex(digest(b"")),
            "da39a3ee5e6b4b0d3255bfef95601890afd80709",
            "the empty message"
        );
        assert_eq!(
            hex(digest(b"abc")),
            "a9993e364706816aba3e25717850c26c9cd0d89d",
            "one block, short"
        );
        assert_eq!(
            hex(digest(
                b"abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq"
            )),
            "84983e441c3bd26ebaae4aa1f95129e5e54670f1",
            "a 56-byte message, which is exactly the length that needs a second block"
        );
    }

    #[test]
    fn a_message_longer_than_one_block_is_hashed_in_sequence() {
        // A million 'a' is the classic vector; a thousand is enough to prove the
        // loop over whole blocks runs and the tail is handled after it, without
        // spending a second of test time on it.
        assert_eq!(
            hex(digest(&[b'a'; 1000])),
            "291e9a6c66994949b57ba5e650361e98fc36b1ba",
            "a message spanning many blocks"
        );
    }

    #[test]
    fn the_lengths_around_a_block_boundary_all_hash_distinctly() {
        // 55, 56 and 64 are the three cases the padding branches split on: room
        // for the length, no room for the length, and an exactly full block.
        // Nothing here checks a published value — the claim is only that the
        // branches are reached and do not collide.
        let hashes: Vec<String> = [55usize, 56, 63, 64, 65]
            .into_iter()
            .map(|length| hex(digest(&vec![b'x'; length])))
            .collect();
        let mut unique = hashes.clone();
        unique.sort();
        unique.dedup();
        assert_eq!(unique.len(), hashes.len(), "two lengths hashed the same");
    }
}
