//! Key material, and the two properties that make it different from bytes.
//!
//! # It erases itself, and the compiler is not allowed to skip that
//!
//! A key wiped by hand is a key that may not be wiped at all: writing zeroes
//! into a buffer nothing afterwards reads is a dead store, and a compiler is
//! entitled to delete it. The wipe here goes through [`zeroize`], whose entire
//! reason to exist is to make that deletion illegal — so a key that goes out of
//! scope is gone from the process image rather than merely unreachable.
//!
//! # It cannot be printed by accident
//!
//! [`SecretBytes`] renders as a fixed marker under `Debug` and implements no
//! `Display` at all. That is not decoration. A key reaches a log, a trace, a
//! panic message or an error body by exactly one route — somebody formatted a
//! struct that contained it — and the containing struct is usually derived
//! `Debug` by a person who never thought about the field. Making the leaf
//! unprintable makes every container safe without anyone having to notice.
//!
//! The one way to the bytes is [`SecretBytes::expose`], named so that
//! `grep -rn "\.expose()"` is a complete list of the places plaintext key
//! material is handled.

use zeroize::{Zeroize, ZeroizeOnDrop};

use crate::error::{Error, Result};

/// How many bytes every key in the hierarchy is.
///
/// One width for all four levels, because they are all ChaCha20-Poly1305 keys:
/// the unseal key derived from a passphrase, the store's master key, a vault's
/// key, and a record's data key. A hierarchy with two widths would need a reason
/// and there is none.
pub const KEY_BYTES: usize = 32;

/// Key material that erases itself and refuses to be printed.
#[derive(Clone, Zeroize, ZeroizeOnDrop)]
pub struct SecretBytes([u8; KEY_BYTES]);

impl SecretBytes {
    /// Fresh key material from the operating system's entropy.
    ///
    /// Every key in the hierarchy starts here. A failure is returned rather than
    /// worked around: a store that carries on after its entropy source refused
    /// would be generating keys from whatever the buffer happened to hold.
    pub fn generate() -> Result<Self> {
        let mut bytes = [0_u8; KEY_BYTES];
        getrandom::fill(&mut bytes).map_err(|_| Error::Entropy)?;
        Ok(Self(bytes))
    }

    /// Adopt bytes that are already key material.
    ///
    /// Used by the key-derivation and unwrapping paths, which produce the bytes
    /// themselves. The argument is taken by value and zeroized here, so the
    /// caller's copy does not outlive the call.
    #[must_use]
    pub fn adopt(mut bytes: [u8; KEY_BYTES]) -> Self {
        let held = Self(bytes);
        bytes.zeroize();
        held
    }

    /// The bytes.
    ///
    /// Deliberately verbose. Every call site is a place plaintext key material
    /// is in hand, and the name is what makes them enumerable.
    #[must_use]
    pub fn expose(&self) -> &[u8; KEY_BYTES] {
        &self.0
    }
}

impl core::fmt::Debug for SecretBytes {
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        // No length, no prefix, no fingerprint. A fingerprint is a stable
        // identifier for a key, which is a thing worth having and is what
        // `KeyId` is for — deriving one here would put it in every log line
        // that formats a surrounding struct, which is the leak this type
        // exists to prevent.
        formatter.write_str("SecretBytes(<redacted>)")
    }
}

/// A stable, public name for a key.
///
/// Carried in every envelope so that the key which opens a value can be found
/// without trying them all, and so that a later key rotation is additive rather
/// than a format change. It is not derived from the key: a value derived from
/// key material is a key-material oracle, however weak, and there is no reason
/// to accept even a weak one when a random identifier costs the same.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug, Hash)]
pub struct KeyId([u8; Self::BYTES]);

impl KeyId {
    /// How many bytes a key identifier is.
    pub const BYTES: usize = 16;

    /// A fresh identifier.
    pub fn generate() -> Result<Self> {
        let mut bytes = [0_u8; Self::BYTES];
        getrandom::fill(&mut bytes).map_err(|_| Error::Entropy)?;
        Ok(Self(bytes))
    }

    /// Adopt an identifier read back from storage.
    #[must_use]
    pub const fn adopt(bytes: [u8; Self::BYTES]) -> Self {
        Self(bytes)
    }

    /// The identifier's bytes.
    #[must_use]
    pub const fn bytes(&self) -> &[u8; Self::BYTES] {
        &self.0
    }
}
