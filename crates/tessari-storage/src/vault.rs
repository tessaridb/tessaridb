//! Whether this process can open anything, and the record that lets it try.
//!
//! # Why the keyring is shared and the root record is not
//!
//! The root record — a salt and the store's master key sealed under a
//! passphrase — is catalog state: it must reach every node, survive a restore,
//! and be identical everywhere, so it travels in the log like a table
//! definition. That is the same split ADR-0018 makes between who else is here
//! and who this node is.
//!
//! The **unsealed master key** is the other half, and it must never travel
//! anywhere. It is per-process, held behind a lock beside the snapshot registry
//! and the consumer registry, and it is not persisted, not replicated and not
//! backed up. A follower that receives every byte of the leader's log receives
//! nothing that opens a secret.
//!
//! # A restart seals the store
//!
//! Nothing here survives the process. That is the property the design paid for
//! when it decided the server may decrypt while unsealed: a node that comes back
//! cannot open anything, for anybody, including its operator, until a passphrase
//! is presented again. It is also the property that makes an unattended restart
//! impossible, which is a real operational cost and is written down rather than
//! discovered.

use std::sync::RwLock;

use tessari_vault::{Keyring, Root, SecretBytes};

use crate::error::{Error, Result};

/// This process's view of whether the store is open.
///
/// `Default` is sealed, which is the safe direction: a keyring that turns up
/// somewhere by default cannot accidentally be an unsealed one.
#[derive(Default, Debug)]
pub struct OpenVault {
    keyring: RwLock<Keyring>,
}

impl OpenVault {
    /// A sealed keyring.
    #[must_use]
    pub fn sealed() -> Self {
        Self::default()
    }

    /// Whether the store is sealed in this process.
    ///
    /// A poisoned lock reads as **sealed**. A panic while the keyring was being
    /// written leaves this process unable to say what it holds, and the safe
    /// answer to "can you open secrets" when you do not know is no.
    #[must_use]
    pub fn is_sealed(&self) -> bool {
        self.keyring
            .read()
            .map_or(true, |keyring| keyring.is_sealed())
    }

    /// Unseal with a passphrase, against the store's root record.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Vault`] when the passphrase does not open the record, or
    /// when the store is already unsealed.
    pub fn unseal(&self, root: &Root, passphrase: &str) -> Result<()> {
        let mut keyring = self.keyring.write().map_err(|_| Error::VaultUnavailable)?;
        keyring.unseal(root, passphrase).map_err(Error::Vault)
    }

    /// Adopt a master key produced by initialising the store.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Vault`] when the store is already unsealed.
    pub fn adopt(&self, master: SecretBytes) -> Result<()> {
        let mut keyring = self.keyring.write().map_err(|_| Error::VaultUnavailable)?;
        keyring.adopt(master).map_err(Error::Vault)
    }

    /// Seal the store.
    ///
    /// # Errors
    ///
    /// Returns [`Error::VaultUnavailable`] when the lock is poisoned.
    pub fn seal(&self) -> Result<()> {
        let mut keyring = self.keyring.write().map_err(|_| Error::VaultUnavailable)?;
        keyring.seal();
        Ok(())
    }

    /// Do something with the master key, without handing it out.
    ///
    /// A borrow rather than a clone, and a closure rather than a guard: a caller
    /// holding a `SecretBytes` decides for itself how long the key lives, and
    /// the whole point of this type is that nothing outside it does. The key
    /// exists for the duration of one call.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Vault`] carrying `Sealed` when there is no key, and
    /// whatever the closure returns otherwise.
    pub fn with_master<T>(&self, act: impl FnOnce(&SecretBytes) -> Result<T>) -> Result<T> {
        let keyring = self.keyring.read().map_err(|_| Error::VaultUnavailable)?;
        let master = keyring.master().map_err(Error::Vault)?;
        act(master)
    }
}
