//! The key hierarchy: four levels, each naming its wrapper and its unwrapper.
//!
//! ```text
//! passphrase --Argon2id--> unseal key      (never stored, lives for one statement)
//!                              |
//!                              v wraps
//!                          master key      (one per store, memory only while unsealed)
//!                              |
//!                              v wraps
//!                          vault key       (one per DEFINE VAULT)
//!                              |
//!                              v wraps
//!                          data key        (one per record, wrapped once per recipient)
//!                              |
//!                              v encrypts
//!                          the secret field
//! ```
//!
//! # Why four levels and not two
//!
//! Each one buys a property that is expensive to add afterwards, and cheap now.
//!
//! **The unseal key is separate from the master key** so the passphrase can
//! change without re-encrypting a byte. Derive the master key straight from the
//! passphrase and a rotation becomes a full-store rewrite — which is how a
//! rotation policy turns into a rotation that never happens.
//!
//! **A key per vault** makes dropping a vault a *crypto-shred*: destroy one
//! wrapped key and every item in it is unopenable in every backup, snapshot and
//! replica that will ever be restored. Without it, dropping a vault is a row
//! delete, which is a statement about the live table and not about the data.
//!
//! **A key per record**, wrapped once per recipient, is what leaves sharing
//! buildable above without this layer knowing what sharing is. Adding a
//! recipient is adding one wrapped key, and no plaintext is needed to do it.

use argon2::{Algorithm, Argon2, Params, Version};
use zeroize::Zeroize;

use crate::envelope::{self, Binding, Level};
use crate::error::{Error, Result};
use crate::secret::{KEY_BYTES, KeyId, SecretBytes};

/// How much memory the key derivation uses, in kibibytes.
///
/// The OWASP floor for Argon2id, and the same figure this store already uses for
/// user passwords. It is a floor and not a default: lowering it is forbidden,
/// because the whole security of an unseal is that a stolen root record cannot
/// be brute-forced offline, and that is the only parameter deciding how slowly.
pub const DERIVE_MEMORY_KIB: u32 = 19_456;

/// How many passes the key derivation makes.
pub const DERIVE_PASSES: u32 = 2;

/// How many lanes the key derivation uses.
pub const DERIVE_LANES: u32 = 1;

/// How many bytes of salt the root record carries.
pub const SALT_BYTES: usize = 16;

/// What a store must persist to be unsealable — and all it must persist.
///
/// Holding this gives an attacker nothing but an offline guessing problem
/// against Argon2id at the parameters above. It contains no key: the salt is
/// public by design, and the master key inside `wrapped` is encrypted under a
/// key that exists only while somebody is typing the passphrase.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Root {
    /// The salt the unseal key is derived with.
    pub salt: [u8; SALT_BYTES],
    /// The identifier of the master key inside `wrapped`.
    pub key_id: KeyId,
    /// The master key, sealed under the key derived from the passphrase.
    pub wrapped: Vec<u8>,
}

impl Root {
    /// Create a store's root record and its master key.
    ///
    /// Returns both because the caller needs the master key now — the store is
    /// unsealed by the act of initialising it — and needs the root record to
    /// persist. The master key is never derivable from the record alone.
    pub fn create(passphrase: &str) -> Result<(Self, SecretBytes)> {
        let mut salt = [0_u8; SALT_BYTES];
        getrandom::fill(&mut salt).map_err(|_| Error::Entropy)?;

        let master = SecretBytes::generate()?;
        let key_id = KeyId::generate()?;
        let unseal = derive(passphrase, &salt)?;
        let wrapped = envelope::seal(
            &unseal,
            key_id,
            &Binding::Key {
                level: Level::Master,
                scope: &salt,
            },
            master.expose(),
        )?;

        Ok((
            Self {
                salt,
                key_id,
                wrapped,
            },
            master,
        ))
    }

    /// Recover the master key from this record and a passphrase.
    ///
    /// A wrong passphrase produces [`Error::WrongKey`], the same answer every
    /// other failed authentication produces, so a caller learns only that it did
    /// not work.
    pub fn unlock(&self, passphrase: &str) -> Result<SecretBytes> {
        let unseal = derive(passphrase, &self.salt)?;
        envelope::open_key(
            &unseal,
            &Binding::Key {
                level: Level::Master,
                scope: &self.salt,
            },
            &self.wrapped,
        )
    }
}

/// Turn a passphrase into key material.
///
/// Argon2id at the parameters above. The output goes straight into the
/// self-erasing type and the intermediate buffer is wiped, so the derived key
/// does not outlive this function anywhere but in the value returned.
pub fn derive(passphrase: &str, salt: &[u8]) -> Result<SecretBytes> {
    let params = Params::new(
        DERIVE_MEMORY_KIB,
        DERIVE_PASSES,
        DERIVE_LANES,
        Some(KEY_BYTES),
    )
    .map_err(|_| Error::Derivation)?;
    let argon = Argon2::new(Algorithm::Argon2id, Version::V0x13, params);

    let mut derived = [0_u8; KEY_BYTES];
    let outcome = argon.hash_password_into(passphrase.as_bytes(), salt, &mut derived);
    let held = outcome
        .map(|()| SecretBytes::adopt(derived))
        .map_err(|_| Error::Derivation);
    derived.zeroize();
    held
}

/// A key sealed under the key above it, with the identifier that names it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Wrapped {
    /// The identifier of the key inside.
    pub key_id: KeyId,
    /// The sealed bytes.
    pub sealed: Vec<u8>,
}

/// Wrap a fresh key for one scope, under the key above it.
///
/// Generates the key as well as wrapping it, because a caller that generates its
/// own would have a plaintext key in hand for longer than the call, and there is
/// no use for one.
pub fn wrap_fresh(
    under: &SecretBytes,
    level: Level,
    scope: &[u8],
) -> Result<(Wrapped, SecretBytes)> {
    let key = SecretBytes::generate()?;
    let wrapped = wrap(under, level, scope, &key)?;
    Ok((wrapped, key))
}

/// Wrap a key that already exists — the second and later recipients of one.
///
/// This is what makes sharing a *write* rather than a decryption: the same data
/// key is sealed again under another party's key, and nothing needs to open the
/// record to do it.
pub fn wrap(under: &SecretBytes, level: Level, scope: &[u8], key: &SecretBytes) -> Result<Wrapped> {
    let key_id = KeyId::generate()?;
    let sealed = envelope::seal(under, key_id, &Binding::Key { level, scope }, key.expose())?;
    Ok(Wrapped { key_id, sealed })
}

/// Recover a wrapped key.
pub fn unwrap(
    under: &SecretBytes,
    level: Level,
    scope: &[u8],
    wrapped: &Wrapped,
) -> Result<SecretBytes> {
    envelope::open_key(under, &Binding::Key { level, scope }, &wrapped.sealed)
}
