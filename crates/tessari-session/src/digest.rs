//! `crypto::sha256` and `crypto::sha512`, and what they are deliberately not.
//!
//! # Text in, text out
//!
//! A digest is compared against a stored one, written beside a record, and read
//! in a log. All three want the sixty-four or hundred-and-twenty-eight
//! lowercase hexadecimal characters every other tool prints, so that is what
//! these answer. The alternative — an array of thirty-two numbers — would make
//! `crypto::sha256(x) = '…'` unwritable, and that comparison is the whole
//! reason the function is in the language.
//!
//! # Why the argument must be text
//!
//! The digest of a number would have to be the digest of *some* rendering of
//! it, and this store's numbers render in three kinds that compare equal:
//! `3`, `3.0` and the decimal `3.0` are one value and would be three digests.
//! Hashing "the canonical text of any value" therefore promises a stability
//! across kinds that nothing here can keep, and would tie every digest ever
//! stored to today's rendering.
//!
//! So the argument is text, refused otherwise by the same `text_at` every
//! string function uses — affordable because the language already holds the
//! sentence a caller should write instead, `crypto::sha256(type::string(x))`,
//! and `type::string` is total.
//!
//! An absent or null argument never reaches here at all: [`crate::call`] answers
//! `NONE` for the whole call before dispatching, which is the convention every
//! other one-argument function follows.
//!
//! # Not a password hash
//!
//! SHA-2 is fast, and fast is the property a stored credential must not have.
//! Passwords go through [`crate::identity`], whose cost parameters are pinned
//! and whose salt is per-record. There is deliberately no callable-from-a-query
//! path to it: a `crypto::argon2`-shaped function would put a credential
//! primitive behind a grant check rather than behind the code that owns the
//! credential (Q-218).

use sha2::{Digest as _, Sha256, Sha512};
use tessari_types::Value;

/// The SHA-256 digest of this text's UTF-8 bytes, as lowercase hex.
///
/// Infallible, and no span: the one thing that can go wrong with a call to
/// these is the argument's kind, and that is refused by `text_at` in the caller
/// before anything reaches here.
pub(crate) fn sha256(text: &str) -> Value {
    hex(&Sha256::digest(text.as_bytes()))
}

/// The SHA-512 digest of this text's UTF-8 bytes, as lowercase hex.
pub(crate) fn sha512(text: &str) -> Value {
    hex(&Sha512::digest(text.as_bytes()))
}

/// Bytes as lowercase hexadecimal text.
///
/// Written out rather than reached for in a crate: it is one loop, and the
/// formatting width is the only thing that can be wrong with it — which is
/// exactly what the tests below pin, because `{:x}` on a byte under sixteen
/// drops the leading zero and shortens the digest without failing.
fn hex(bytes: &[u8]) -> Value {
    let mut text = String::with_capacity(bytes.len().saturating_mul(2));
    for byte in bytes {
        text.push(nibble(byte >> 4));
        text.push(nibble(byte & 0x0f));
    }
    Value::from(text.as_str())
}

/// One half-byte as its hexadecimal digit.
///
/// Saturating rather than bare arithmetic, which the workspace denies: the
/// caller only ever passes a masked four-bit value, so no addition here can
/// reach a `u8`'s limit, and the saturating form says that without asking the
/// reader to go and check the caller.
fn nibble(half: u8) -> char {
    match half {
        0..=9 => char::from(b'0'.saturating_add(half)),
        _ => char::from(b'a'.saturating_add(half.saturating_sub(10))),
    }
}

#[cfg(test)]
#[expect(
    clippy::panic,
    clippy::unwrap_used,
    reason = "a digest test that cannot fail loudly asserts nothing"
)]
mod tests {
    use super::{Value, sha256, sha512};

    fn text(value: Value) -> String {
        let Value::String(held) = value else {
            panic!("a digest must answer with text, got {value:?}")
        };
        held
    }

    /// The published vectors, which is the only way to know this is SHA-2.
    ///
    /// An implementation that agrees with itself proves nothing, and every
    /// property below — length, alphabet, avalanche — is satisfied by a hash
    /// that is not SHA-256 at all. These two strings and their digests are in
    /// the standard.
    #[test]
    fn the_empty_string_and_abc_hash_to_their_published_values() {
        assert_eq!(
            text(sha256("")),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(
            text(sha256("abc")),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert_eq!(
            text(sha512("")),
            "cf83e1357eefb8bdf1542850d66d8007d620e4050b5715dc83f4a921d36ce9ce\
             47d0d13c5d85f2b0ff8318d2877eec2f63b931bd47417a81a538327af927da3e"
        );
        assert_eq!(
            text(sha512("abc")),
            "ddaf35a193617abacc417349ae20413112e6fa4e89a97ea20a9eeee64b55d39a\
             2192992a274fc1a836ba3c23a3feebbd454d4423643ce80e2a9ac94fa54ca49f"
        );
    }

    /// The digest is the bytes of the text, not of anything Rust prints.
    ///
    /// A non-ASCII string is the case that separates hashing the UTF-8 from
    /// hashing an escaped or a debug rendering, and both mistakes produce a
    /// perfectly plausible-looking digest.
    #[test]
    fn text_beyond_ascii_hashes_its_utf8_bytes() {
        // The published digest of the three bytes of `é` in UTF-8.
        assert_eq!(
            text(sha256("é")),
            text(sha256(std::str::from_utf8(&[0xc3, 0xa9]).unwrap()))
        );
        assert_ne!(text(sha256("é")), text(sha256("e")));
    }

    /// The hex rendering keeps every leading zero.
    ///
    /// `{:x}` on a byte below sixteen prints one character, which shortens the
    /// digest silently and makes two different digests collide in text. The
    /// first vector above begins with `e3` and would not catch it; this asserts
    /// the widths directly, over inputs until a zero byte has certainly
    /// occurred.
    #[test]
    fn every_byte_becomes_exactly_two_characters() {
        for at_index in 0..64_u32 {
            let held = text(sha256(&at_index.to_string()));
            assert_eq!(held.len(), 64, "sha256 of {at_index} was {held}");
            assert!(
                held.chars()
                    .all(|digit| digit.is_ascii_hexdigit() && !digit.is_ascii_uppercase()),
                "not lowercase hex: {held}"
            );
            assert_eq!(text(sha512(&at_index.to_string())).len(), 128);
        }
    }
}
