//! The sealed value: its bytes on disk, and the one place this crate encrypts.
//!
//! # The layout
//!
//! ```text
//! [version:1][algorithm:1][key id:16][nonce:12][ciphertext + tag:…]
//! ```
//!
//! A version and an algorithm identifier ride in every value, because a format
//! with neither is a format that cannot be migrated: the first value written is
//! then the last format the store may ever use. The key identifier is what lets
//! the opener find the one key that works instead of trying every key it holds,
//! and it is what makes a future rotation additive.
//!
//! **The whole header is authenticated.** It goes into the associated data
//! alongside the binding, so an attacker cannot rewrite the algorithm byte to a
//! weaker one, or swap in another key's identifier, without the value failing to
//! open. A header outside the authentication is a header an attacker owns.
//!
//! # Why a value is bound to its place
//!
//! Encryption alone says the bytes are secret; it says nothing about *where they
//! belong*. Without a binding, a ciphertext lifted from `staff:1.salary` and
//! written into `staff:2.salary` opens perfectly and the store has been lied to
//! in its own bytes, with every checksum intact. [`Binding`] is the fix: the
//! record, the field and the level of the key are authenticated with the value,
//! so a ciphertext only opens in the place it was sealed.
//!
//! Every component is **length-prefixed** before it is hashed in. Concatenating
//! `"ab"` with `"c"` and `"a"` with `"bc"` produces identical bytes, and two
//! different places would then share one binding — which is the whole property
//! being bought, lost to a detail.

use chacha20poly1305::aead::{Aead, Payload};
use chacha20poly1305::{ChaCha20Poly1305, KeyInit, Nonce};

use crate::error::{Error, Result};
use crate::secret::{KEY_BYTES, KeyId, SecretBytes};

/// The format version every value written by this build carries.
pub const VERSION: u8 = 1;

/// ChaCha20-Poly1305, the only algorithm this build carries.
pub const ALGORITHM_CHACHA20_POLY1305: u8 = 1;

/// How many bytes a nonce is.
pub const NONCE_BYTES: usize = 12;

/// How many bytes the authentication tag adds.
pub const TAG_BYTES: usize = 16;

/// How many bytes precede the ciphertext.
pub const HEADER_BYTES: usize = 1 + 1 + KeyId::BYTES + NONCE_BYTES;

/// Which level of the hierarchy a wrapped key belongs to.
///
/// Part of the binding so that a wrapped vault key cannot be presented as a
/// wrapped data key, or the reverse. The levels are otherwise
/// indistinguishable — all four are thirty-two random bytes — and an attacker
/// who can move one where another is expected can rearrange the hierarchy
/// without breaking a single authentication tag.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Level {
    /// The store's master key, wrapped under the key derived from the unseal
    /// passphrase.
    Master,
    /// One vault's key, wrapped under the master key.
    Vault,
    /// One record's data key, wrapped under a vault key or a recipient's key.
    Data,
}

impl Level {
    const fn tag(self) -> u8 {
        match self {
            Self::Master => 1,
            Self::Vault => 2,
            Self::Data => 3,
        }
    }
}

/// What a sealed value is bound to.
///
/// Authenticated with the value, never encrypted — the point is that changing
/// any of it makes the value refuse to open, not that it is hidden. The record
/// identifier and the field name are already visible in the key.
#[derive(Clone, Copy, Debug)]
pub enum Binding<'a> {
    /// One field of one record.
    Field {
        /// The table the record belongs to.
        table: u64,
        /// The record's identifier, as its stored bytes.
        record: &'a [u8],
        /// The declared name of the field.
        field: &'a str,
    },
    /// A key wrapped under the key above it.
    Key {
        /// Which level of the hierarchy the wrapped key sits at.
        level: Level,
        /// What the wrapped key belongs to — a vault's identifier, a record's
        /// identifier, or the recipient the copy was wrapped for.
        scope: &'a [u8],
    },
}

impl Binding<'_> {
    /// The binding as authenticated bytes.
    ///
    /// A leading tag separates the two shapes, then every component is written
    /// with its length ahead of it, so no two distinct bindings can produce the
    /// same bytes.
    fn associated(&self) -> Result<Vec<u8>> {
        let mut bytes = Vec::new();
        match self {
            Self::Field {
                table,
                record,
                field,
            } => {
                bytes.push(0);
                bytes.extend_from_slice(&table.to_be_bytes());
                push_framed(&mut bytes, record)?;
                push_framed(&mut bytes, field.as_bytes())?;
            }
            Self::Key { level, scope } => {
                bytes.push(1);
                bytes.push(level.tag());
                push_framed(&mut bytes, scope)?;
            }
        }
        Ok(bytes)
    }
}

/// Append a component with its length ahead of it.
///
/// The length is refused rather than truncated when it does not fit: a `as u32`
/// here would make a component longer than four gigabytes share a framing with a
/// shorter one, which is the ambiguity the framing exists to remove.
fn push_framed(into: &mut Vec<u8>, component: &[u8]) -> Result<()> {
    let length = u32::try_from(component.len()).map_err(|_| Error::NotSealed)?;
    into.extend_from_slice(&length.to_be_bytes());
    into.extend_from_slice(component);
    Ok(())
}

/// Seal a value under a key, bound to its place.
///
/// The nonce is fresh from the operating system for every call. With a distinct
/// data key per record and a handful of fields in a record, a random ninety-six
/// bit nonce is nowhere near its birthday bound; a counter would need durable
/// per-key state, and a counter that resets after a crash repeats a nonce, which
/// is the one failure this construction does not survive.
pub fn seal(
    key: &SecretBytes,
    key_id: KeyId,
    binding: &Binding<'_>,
    plaintext: &[u8],
) -> Result<Vec<u8>> {
    let mut nonce = [0_u8; NONCE_BYTES];
    getrandom::fill(&mut nonce).map_err(|_| Error::Entropy)?;

    // Saturating rather than checked: this is a capacity hint, so the honest
    // behaviour at the impossible end is to ask for less and let the vector
    // grow, not to refuse a value that would otherwise seal.
    let capacity = HEADER_BYTES
        .saturating_add(plaintext.len())
        .saturating_add(TAG_BYTES);
    let mut sealed = Vec::with_capacity(capacity);
    sealed.push(VERSION);
    sealed.push(ALGORITHM_CHACHA20_POLY1305);
    sealed.extend_from_slice(key_id.bytes());
    sealed.extend_from_slice(&nonce);

    let mut associated = sealed.clone();
    associated.extend_from_slice(&binding.associated()?);

    let cipher = ChaCha20Poly1305::new_from_slice(key.expose()).map_err(|_| Error::WrongKey)?;
    let ciphertext = cipher
        .encrypt(
            &Nonce::from(nonce),
            Payload {
                msg: plaintext,
                aad: &associated,
            },
        )
        .map_err(|_| Error::WrongKey)?;

    sealed.extend_from_slice(&ciphertext);
    Ok(sealed)
}

/// The identifier of the key that opens this value.
///
/// Read without authenticating anything, which is exactly what it is for: the
/// opener needs to know which key to fetch before it can authenticate. The
/// identifier is itself authenticated during [`open`], so a rewritten one turns
/// into a refusal rather than into the wrong key being trusted.
pub fn key_id_of(sealed: &[u8]) -> Result<KeyId> {
    let header = header_of(sealed)?;
    Ok(header.key_id)
}

/// Open a value sealed under this key and bound to this place.
///
/// A wrong key, altered bytes, truncated bytes, and a value lifted from another
/// record all end here with [`Error::WrongKey`] — one answer, so that a caller
/// cannot learn which of its guesses was closer.
pub fn open(key: &SecretBytes, binding: &Binding<'_>, sealed: &[u8]) -> Result<Vec<u8>> {
    let header = header_of(sealed)?;

    let mut associated = sealed.get(..HEADER_BYTES).ok_or(Error::NotSealed)?.to_vec();
    associated.extend_from_slice(&binding.associated()?);

    let ciphertext = sealed.get(HEADER_BYTES..).ok_or(Error::NotSealed)?;

    let cipher = ChaCha20Poly1305::new_from_slice(key.expose()).map_err(|_| Error::WrongKey)?;
    cipher
        .decrypt(
            &Nonce::from(header.nonce),
            Payload {
                msg: ciphertext,
                aad: &associated,
            },
        )
        .map_err(|_| Error::WrongKey)
}

/// Open a value that is itself a key.
///
/// A convenience with a purpose: unwrapping produces key material, and the
/// natural spelling — open into a `Vec<u8>`, copy into an array — leaves a
/// plaintext key in a heap buffer that nothing erases. This copies into the
/// self-erasing type and wipes the intermediate.
pub fn open_key(key: &SecretBytes, binding: &Binding<'_>, sealed: &[u8]) -> Result<SecretBytes> {
    use zeroize::Zeroize;

    let mut opened = open(key, binding, sealed)?;
    let held = <[u8; KEY_BYTES]>::try_from(opened.as_slice())
        .map(SecretBytes::adopt)
        .map_err(|_| Error::WrongKey);
    opened.zeroize();
    held
}

/// The parsed header of a sealed value.
struct Header {
    key_id: KeyId,
    nonce: [u8; NONCE_BYTES],
}

/// Parse and check the header without authenticating anything.
fn header_of(sealed: &[u8]) -> Result<Header> {
    if sealed.len() < HEADER_BYTES + TAG_BYTES {
        return Err(Error::NotSealed);
    }
    let version = *sealed.first().ok_or(Error::NotSealed)?;
    if version != VERSION {
        return Err(Error::UnknownVersion(version));
    }
    let algorithm = *sealed.get(1).ok_or(Error::NotSealed)?;
    if algorithm != ALGORITHM_CHACHA20_POLY1305 {
        return Err(Error::UnknownAlgorithm(algorithm));
    }
    let key_id = sealed
        .get(2..2 + KeyId::BYTES)
        .and_then(|bytes| <[u8; KeyId::BYTES]>::try_from(bytes).ok())
        .map(KeyId::adopt)
        .ok_or(Error::NotSealed)?;
    let nonce = sealed
        .get(2 + KeyId::BYTES..HEADER_BYTES)
        .and_then(|bytes| <[u8; NONCE_BYTES]>::try_from(bytes).ok())
        .ok_or(Error::NotSealed)?;
    Ok(Header { key_id, nonce })
}
