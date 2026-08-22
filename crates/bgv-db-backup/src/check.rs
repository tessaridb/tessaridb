//! A checksum over each record, and what it is for.
//!
//! # A length is not an integrity check
//!
//! Framing catches a file that was **cut**: the last record is short and the
//! reader says so. It catches nothing about a file that is the right length and
//! holds the wrong bytes — a flipped bit in the middle of a payload, a page that
//! came back from a bad disk, an object store that returned a stale copy. Such a
//! record either fails to decode, which is luck, or decodes into a *different
//! record* and is applied without a word.
//!
//! A backup is read on the worst day somebody has, and "it applied cleanly" is
//! the one thing they will not re-check. So each record carries a CRC-32 of its
//! body, and a backup can be verified without being applied to anything.
//!
//! CRC-32 rather than a cryptographic digest, deliberately: this detects
//! **corruption**, which is what happens to files, and it does not pretend to
//! detect **tampering**, which needs a key and a threat model this format does
//! not have. Saying which of the two is being claimed matters more than the
//! strength of the function.

/// The IEEE polynomial, reflected — the one every other CRC-32 in the world uses.
const POLYNOMIAL: u32 = 0xedb8_8320;

/// The table, built once at compile time.
const TABLE: [u32; 256] = table();

const fn table() -> [u32; 256] {
    let mut held = [0_u32; 256];
    // Two counters rather than one and a conversion. `usize::try_from` is not
    // available in a `const fn` and an `as` cast is what this workspace denies,
    // so the index and the value it starts from are counted side by side —
    // which costs a line and leaves nothing to reason about.
    let mut at = 0_usize;
    let mut seed = 0_u32;
    while at < 256 {
        let mut value = seed;
        let mut round = 0_u32;
        while round < 8 {
            value = if value & 1 == 1 {
                (value >> 1) ^ POLYNOMIAL
            } else {
                value >> 1
            };
            round = round.saturating_add(1);
        }
        held[at] = value;
        at = at.saturating_add(1);
        seed = seed.saturating_add(1);
    }
    held
}

/// The CRC-32 of these bytes.
#[must_use]
pub(crate) fn crc32(bytes: &[u8]) -> u32 {
    let mut held = u32::MAX;
    for byte in bytes {
        // The low byte of the running value, mixed with the input byte. Masked
        // and converted rather than cast: the mask makes the conversion total,
        // so there is no truncation to reason about.
        let at = usize::try_from((held ^ u32::from(*byte)) & 0xff).unwrap_or(0);
        let entry = TABLE.get(at).copied().unwrap_or(0);
        held = (held >> 8) ^ entry;
    }
    held ^ u32::MAX
}

#[cfg(test)]
mod tests {
    use super::crc32;

    #[test]
    fn it_is_the_crc32_everybody_elses_tools_compute() {
        // The two check values every CRC-32 implementation is measured against,
        // so this can be compared with `cksum`, `zlib` or a Go program without
        // anybody having to trust it.
        assert_eq!(crc32(b""), 0x0000_0000);
        assert_eq!(crc32(b"123456789"), 0xcbf4_3926);
        assert_eq!(crc32(b"a"), 0xe8b7_be43);
    }

    #[test]
    fn one_flipped_bit_changes_it() {
        let held = crc32(b"the salary spreadsheet");
        let flipped = crc32(b"the salary spreadshees");
        assert_ne!(held, flipped);
    }
}
