//! CRC-32 (IEEE 802.3), for the gzip trailer.
//!
//! A gzip member closes with the CRC-32 of what it decompressed to. Checked,
//! because a payload that inflates cleanly can still be the wrong payload, and
//! this is the one place gzip says so. Twenty lines rather than a dependency.

/// The reflected IEEE polynomial.
const POLYNOMIAL: u32 = 0xEDB8_8320;

const TABLE: [u32; 256] = {
    let mut table = [0_u32; 256];
    // Two counters walking together, because a `const` block has no checked
    // conversion from the slot's index to the value it stands for.
    let mut index = 0;
    let mut value: u32 = 0;
    while index < 256 {
        let mut crc = value;
        let mut bit = 0;
        while bit < 8 {
            crc = if crc & 1 == 1 {
                (crc >> 1) ^ POLYNOMIAL
            } else {
                crc >> 1
            };
            bit += 1;
        }
        table[index] = crc;
        index += 1;
        value += 1;
    }
    table
};

/// The CRC-32 of `bytes`.
#[must_use]
pub fn ieee(bytes: &[u8]) -> u32 {
    !bytes.iter().fold(u32::MAX, |crc, byte| {
        let slot = usize::from(u8::try_from(crc & 0xff).unwrap_or(0) ^ byte);
        TABLE.get(slot).copied().unwrap_or(0) ^ crc.wrapping_shr(8)
    })
}
