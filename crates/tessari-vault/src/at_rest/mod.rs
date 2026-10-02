//! The key to the data at rest (ADR-0108 D7): what an operator hands a node so
//! that nothing it writes to disk — the engine's files and every backup — is
//! readable without it.
//!
//! # One key, three uses, never the same bytes twice
//!
//! The operator gives one 32-byte key. It is never used directly: three subkeys
//! are drawn from it by HKDF-SHA256 under their own labels, one for the
//! engine's files, one for backups and one for the marker that tells a store
//! which key it was written under. A key used for two purposes under two
//! constructions is a key whose security rests on the two never interacting;
//! separate subkeys make that a property rather than an argument.
//!
//! # What it does not defend against
//!
//! A running node holds the key in memory and serves plaintext to anyone with a
//! credential; privileged code on the host can read both. What it defends is the
//! disk and the backup — a stolen volume, a discarded drive, a backup file in a
//! bucket. The engine's files are encrypted with a stream cipher and rely on the
//! engine's own block checksums for integrity; backups are authenticated end to
//! end. Rotating the key means writing the store again under a new one (a
//! backup restored into a new store); nothing rewrites a store in place.

mod stream;

use std::path::Path;

use chacha20poly1305::aead::{Aead, Payload};
use chacha20poly1305::{ChaCha20Poly1305, KeyInit, Nonce};
use zeroize::Zeroize as _;

pub use stream::{Opening, Reading, SEALED_MAGIC, Sealing, reading};

use crate::error::{Error, Result};
use crate::secret::{KEY_BYTES, SecretBytes};

/// The subkey labels. Changing one is changing the key every existing store
/// and backup was written under.
const ENGINE_LABEL: &[u8] = b"tessaridb at-rest v1 engine files";
const BACKUP_LABEL: &[u8] = b"tessaridb at-rest v1 backups";
const CHECK_LABEL: &[u8] = b"tessaridb at-rest v1 check";

/// What the marker seals, and binds as associated data, so a marker from
/// another purpose cannot pass for one.
const MARKER_TEXT: &[u8] = b"tessaridb encrypted store v1";
const MARKER_NONCE_BYTES: usize = 12;

/// The key to a node's data at rest, split into its three subkeys.
pub struct AtRestKey {
    engine: SecretBytes,
    backups: SecretBytes,
    check: SecretBytes,
}

impl core::fmt::Debug for AtRestKey {
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        formatter.write_str("AtRestKey(<redacted>)")
    }
}

impl AtRestKey {
    /// Read the key from the file at `path`.
    ///
    /// The file holds exactly 32 bytes — `openssl rand 32 > key` makes one. On
    /// Unix it is refused when its group or anybody else may read it, the rule
    /// a private SSH key follows, because a key file anyone on the host can
    /// read protects the disk from nobody who can log in to it.
    ///
    /// # Errors
    ///
    /// [`Error::KeyFile`] naming the check that failed.
    pub fn read(path: &Path) -> Result<Self> {
        let refused = |reason: String| Error::KeyFile {
            path: path.display().to_string(),
            reason,
        };
        let metadata = std::fs::metadata(path).map_err(|failure| refused(failure.to_string()))?;
        private(&metadata).map_err(refused)?;
        let mut held = std::fs::read(path).map_err(|failure| refused(failure.to_string()))?;
        let length = held.len();
        let key: Result<[u8; KEY_BYTES]> = held.as_slice().try_into().map_err(|_| {
            refused(format!(
                "holds {length} bytes; a key is exactly {KEY_BYTES} (`openssl rand {KEY_BYTES}` writes one)"
            ))
        });
        held.zeroize();
        Self::from_key(&SecretBytes::adopt(key?))
    }

    /// Split an operator's key into its subkeys.
    ///
    /// # Errors
    ///
    /// [`Error::Derivation`] when HKDF refuses, which it does only for an
    /// output longer than it can expand and these are 32 bytes each.
    pub fn from_key(key: &SecretBytes) -> Result<Self> {
        Ok(Self {
            engine: subkey(key, ENGINE_LABEL)?,
            backups: subkey(key, BACKUP_LABEL)?,
            check: subkey(key, CHECK_LABEL)?,
        })
    }

    /// The key the engine's files are encrypted under.
    #[must_use]
    pub const fn engine(&self) -> &SecretBytes {
        &self.engine
    }

    /// A fresh marker for a store created under this key.
    ///
    /// # Errors
    ///
    /// [`Error::Entropy`] when the system will not supply a nonce.
    pub fn marker(&self) -> Result<Vec<u8>> {
        let mut nonce = [0_u8; MARKER_NONCE_BYTES];
        getrandom::fill(&mut nonce).map_err(|_| Error::Entropy)?;
        let cipher =
            ChaCha20Poly1305::new_from_slice(self.check.expose()).map_err(|_| Error::WrongKey)?;
        let sealed = cipher
            .encrypt(
                &Nonce::from(nonce),
                Payload {
                    msg: MARKER_TEXT,
                    aad: MARKER_TEXT,
                },
            )
            .map_err(|_| Error::WrongKey)?;
        let mut marker = nonce.to_vec();
        marker.extend_from_slice(&sealed);
        Ok(marker)
    }

    /// Whether `marker` was written under this key.
    ///
    /// The authentication is the comparison, so there is no hand-written
    /// equality on key-derived bytes to get wrong.
    ///
    /// # Errors
    ///
    /// [`Error::WrongKey`] when it was written under another key or altered.
    pub fn opens(&self, marker: &[u8]) -> Result<()> {
        let (nonce, sealed) = marker
            .split_first_chunk::<MARKER_NONCE_BYTES>()
            .ok_or(Error::WrongKey)?;
        let cipher =
            ChaCha20Poly1305::new_from_slice(self.check.expose()).map_err(|_| Error::WrongKey)?;
        let opened = cipher
            .decrypt(
                &Nonce::from(*nonce),
                Payload {
                    msg: sealed,
                    aad: MARKER_TEXT,
                },
            )
            .map_err(|_| Error::WrongKey)?;
        if opened == MARKER_TEXT {
            Ok(())
        } else {
            Err(Error::WrongKey)
        }
    }

    /// Seal a backup as it is written into `out`, after writing its head.
    ///
    /// # Errors
    ///
    /// The writer's failure, or the system refusing the file's nonce.
    pub fn seal_into<W: std::io::Write>(&self, out: W) -> std::io::Result<Sealing<W>> {
        Sealing::new(&self.backups, out)
    }

    /// The backup subkey, for the reader.
    pub(crate) const fn backups(&self) -> &SecretBytes {
        &self.backups
    }
}

/// A subkey drawn from `key` under `label`.
fn subkey(key: &SecretBytes, label: &[u8]) -> Result<SecretBytes> {
    let prk = ring::hkdf::Salt::new(ring::hkdf::HKDF_SHA256, &[]).extract(key.expose());
    let mut out = [0_u8; KEY_BYTES];
    prk.expand(&[label], ring::hkdf::HKDF_SHA256)
        .and_then(|okm| okm.fill(&mut out))
        .map_err(|_| Error::Derivation)?;
    Ok(SecretBytes::adopt(out))
}

#[cfg(unix)]
fn private(metadata: &std::fs::Metadata) -> std::result::Result<(), String> {
    use std::os::unix::fs::PermissionsExt as _;
    let mode = metadata.permissions().mode();
    if mode & 0o077 == 0 {
        Ok(())
    } else {
        Err(format!(
            "may be read by others (mode {:o}); `chmod 600` it",
            mode & 0o777
        ))
    }
}

#[cfg(not(unix))]
fn private(_: &std::fs::Metadata) -> std::result::Result<(), String> {
    Ok(())
}

#[cfg(test)]
mod tests;
