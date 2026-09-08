//! `encoding::base64`, `encoding::hex`, and their inverses.
//!
//! # Why the store needs them at all
//!
//! The value system has a `bytes` kind, and until now there was no way to carry
//! one through anything that speaks text. A caller could store bytes and read
//! them back; they could not put them in a JSON field, paste them into a
//! message, or receive them from something that had already encoded them. These
//! four are that road, in both directions.
//!
//! # Written here rather than depended on
//!
//! Both alphabets are fully specified in RFC 4648 and both codecs are a loop
//! whose only failure modes are the ones the tests below pin: the padding, the
//! alphabet, and what an odd or invalid input does. That is the same reasoning
//! `crate::digest` gives for the *opposite* conclusion about SHA-2 — a hash is
//! two hundred lines of bit manipulation nobody can check by reading, and this
//! is not.
//!
//! # A bad string answers `NONE`, and is not refused
//!
//! Decoding asks about a **value**, not about a kind. The kind is checked by
//! the same `text_at` every string function uses, before anything reaches here.
//! What is left is whether that particular text spells bytes, and a table
//! holding one row that does not should narrow rather than become unreadable —
//! which is the rule every `type::` cast already follows.

use tessari_types::Value;

/// The standard alphabet, RFC 4648 §4. Not the URL-safe one, which is a
/// different function nobody has asked for.
const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

/// Bytes as padded standard base64 text.
pub(crate) fn base64(bytes: &[u8]) -> Value {
    let mut out = String::with_capacity(bytes.len().div_ceil(3).saturating_mul(4));
    for chunk in bytes.chunks(3) {
        // Three bytes become four characters of six bits each. A short final
        // chunk is padded with zero bits and then with `=` for the characters
        // those bits never filled, which is what makes the length recoverable.
        let held = chunk.iter().enumerate().fold(0_u32, |held, (at, byte)| {
            held | (u32::from(*byte) << (16_usize.saturating_sub(at.saturating_mul(8))))
        });
        for at in 0..4 {
            if at <= chunk.len() {
                let index = (held >> (18_usize.saturating_sub(at.saturating_mul(6)))) & 0x3f;
                let index = usize::try_from(index).unwrap_or(0);
                out.push(char::from(ALPHABET[index.min(63)]));
            } else {
                out.push('=');
            }
        }
    }
    Value::String(out)
}

/// The bytes that base64 text encodes, or `None` when it encodes none.
pub(crate) fn base64_decode(text: &str) -> Option<Vec<u8>> {
    let trimmed = text.trim_end_matches('=');
    // Padding may only be at the end, and it may only be one or two characters:
    // three would mean a final chunk carrying no bits at all.
    if text.len().checked_rem(4) != Some(0) || text.len().saturating_sub(trimmed.len()) > 2 {
        return None;
    }
    let mut out = Vec::with_capacity(trimmed.len().saturating_mul(3) / 4);
    let mut held = 0_u32;
    let mut bits = 0_u32;
    for character in trimmed.bytes() {
        let index = ALPHABET.iter().position(|held| *held == character)?;
        let index = u32::try_from(index).ok()?;
        held = (held << 6) | index;
        bits = bits.saturating_add(6);
        if bits >= 8 {
            bits = bits.saturating_sub(8);
            let byte = (held >> bits) & 0xff;
            out.push(u8::try_from(byte).ok()?);
        }
    }
    // Whatever is left over is the padding's zero bits, and they must be zero:
    // a trailing group carrying set bits spells a byte the padding says is not
    // there, so two different strings would decode to one value.
    if held & ((1_u32 << bits).saturating_sub(1)) != 0 {
        return None;
    }
    Some(out)
}

/// Bytes as lowercase hexadecimal text.
pub(crate) fn hex(bytes: &[u8]) -> Value {
    let mut out = String::with_capacity(bytes.len().saturating_mul(2));
    for byte in bytes {
        out.push(nibble(byte >> 4));
        out.push(nibble(byte & 0x0f));
    }
    Value::String(out)
}

/// The bytes hexadecimal text spells, or `None`.
pub(crate) fn hex_decode(text: &str) -> Option<Vec<u8>> {
    if text.len().checked_rem(2) != Some(0) {
        return None;
    }
    let held: Vec<u8> = text.bytes().collect();
    let mut out = Vec::with_capacity(held.len() / 2);
    for pair in held.chunks(2) {
        let high = value_of(*pair.first()?)?;
        let low = value_of(*pair.get(1)?)?;
        out.push(byte_of(high, low));
    }
    Some(out)
}

/// One nibble as its lowercase hexadecimal character.
///
/// Written out rather than computed from `b'0'`. Sixteen arms are read at a
/// glance and cannot be off by one, where the arithmetic form has two ranges
/// and an offset between them — which is exactly the shape of the bug the
/// existing hex writer in `crate::digest` documents itself against.
const fn nibble(held: u8) -> char {
    match held {
        0 => '0',
        1 => '1',
        2 => '2',
        3 => '3',
        4 => '4',
        5 => '5',
        6 => '6',
        7 => '7',
        8 => '8',
        9 => '9',
        10 => 'a',
        11 => 'b',
        12 => 'c',
        13 => 'd',
        14 => 'e',
        _ => 'f',
    }
}

/// What one hexadecimal character is worth, in either case.
const fn value_of(character: u8) -> Option<u8> {
    match character {
        b'0'..=b'9' => Some(character.saturating_sub(b'0')),
        b'a'..=b'f' => Some(character.saturating_sub(b'a').saturating_add(10)),
        b'A'..=b'F' => Some(character.saturating_sub(b'A').saturating_add(10)),
        _ => None,
    }
}

/// The high and low nibbles of a byte, recombined.
const fn byte_of(high: u8, low: u8) -> u8 {
    (high << 4) | low
}

#[cfg(test)]
mod tests {
    #![allow(clippy::panic)]

    use super::{base64, base64_decode, hex, hex_decode};
    use tessari_types::Value;

    /// The vectors RFC 4648 §10 prints, which is the only reason to trust a
    /// hand-written codec over a dependency.
    #[test]
    fn the_rfc_4648_vectors_encode_as_the_rfc_says() {
        for (input, expected) in [
            ("", ""),
            ("f", "Zg=="),
            ("fo", "Zm8="),
            ("foo", "Zm9v"),
            ("foob", "Zm9vYg=="),
            ("fooba", "Zm9vYmE="),
            ("foobar", "Zm9vYmFy"),
        ] {
            assert_eq!(
                base64(input.as_bytes()),
                Value::String(expected.to_owned()),
                "base64({input:?})"
            );
            assert_eq!(
                base64_decode(expected).as_deref(),
                Some(input.as_bytes()),
                "base64_decode({expected:?})"
            );
        }
    }

    #[test]
    fn the_rfc_4648_vectors_round_trip_through_hex_too() {
        for (input, expected) in [
            ("", ""),
            ("f", "66"),
            ("fo", "666f"),
            ("foobar", "666f6f626172"),
        ] {
            assert_eq!(hex(input.as_bytes()), Value::String(expected.to_owned()));
            assert_eq!(hex_decode(expected).as_deref(), Some(input.as_bytes()));
        }
    }

    #[test]
    fn every_byte_survives_a_round_trip() {
        // The loop, not a sample: an off-by-one in the shifting would be
        // invisible on ASCII and wrong on the high half.
        let all: Vec<u8> = (0..=255).collect();
        let Value::String(encoded) = base64(&all) else {
            panic!("base64 answered with something that is not text");
        };
        assert_eq!(base64_decode(&encoded).as_deref(), Some(all.as_slice()));
        let Value::String(encoded) = hex(&all) else {
            panic!("hex answered with something that is not text");
        };
        assert_eq!(hex_decode(&encoded).as_deref(), Some(all.as_slice()));
    }

    #[test]
    fn text_that_spells_no_bytes_answers_nothing() {
        // Each of these is a different way to be wrong, and a codec that
        // accepted any of them would decode two strings to one value.
        for bad in ["a", "abc", "Zm9v=", "Zm9vYg=", "!!!!", "Zg=a", "Zh=="] {
            assert_eq!(base64_decode(bad), None, "base64_decode({bad:?})");
        }
        for bad in ["6", "6g6", "zz", "66 6f"] {
            assert_eq!(hex_decode(bad), None, "hex_decode({bad:?})");
        }
    }

    #[test]
    fn hex_is_lowercase_and_reads_either_case() {
        assert_eq!(hex(&[0xab, 0xcd]), Value::String("abcd".to_owned()));
        assert_eq!(hex_decode("ABCD"), hex_decode("abcd"));
    }
}
