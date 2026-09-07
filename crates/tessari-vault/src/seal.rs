//! Sealed and unsealed: the one piece of state that decides whether anything
//! can be opened.
//!
//! # What sealing actually is
//!
//! A sealed store is one where the master key is not in memory. There is no
//! flag consulted by a check somebody has to remember to write — the key is
//! either held or it is `None`, and every path that needs it asks
//! [`Keyring::master`], which cannot answer without one. So "sealed" is a
//! property of what the process holds, not a mode it is in, and a bug that
//! forgets to check it cannot exist because there is nothing to forget.
//!
//! # What restarting does
//!
//! Nothing persists this. A process that restarts comes back sealed, and the
//! store's secrets are unopenable by anybody — including its operator — until a
//! passphrase is presented again. That is the property the design trades
//! unattended restart for, and it is the reason the honest claim about this
//! store is *"a sealed or restarted node cannot open anything"* rather than
//! *"the database cannot read your secrets"*, which decision 2 made false.

use crate::error::{Error, Result};
use crate::keys::Root;
use crate::secret::SecretBytes;

/// The master key, while the store is unsealed.
///
/// `Default` is the sealed state, which is the safe one: a keyring that appears
/// somewhere by default cannot accidentally be an unsealed one.
#[derive(Default, Debug)]
pub struct Keyring {
    master: Option<SecretBytes>,
}

impl Keyring {
    /// A sealed keyring.
    #[must_use]
    pub fn sealed() -> Self {
        Self::default()
    }

    /// Whether the store is sealed.
    #[must_use]
    pub fn is_sealed(&self) -> bool {
        self.master.is_none()
    }

    /// Unseal with a passphrase, against a store's root record.
    ///
    /// Refuses when already unsealed rather than replacing the key. A second
    /// unseal under a different passphrase would otherwise change which values
    /// open, silently, and the store would answer differently before and after
    /// with nothing recording that anything happened.
    pub fn unseal(&mut self, root: &Root, passphrase: &str) -> Result<()> {
        if self.master.is_some() {
            return Err(Error::AlreadyUnsealed);
        }
        self.master = Some(root.unlock(passphrase)?);
        Ok(())
    }

    /// Adopt a master key directly.
    ///
    /// The initialisation path only: creating a store's root record produces the
    /// master key as a by-product, and the store is unsealed by having been
    /// created. Refuses when already unsealed, for the reason [`Keyring::unseal`]
    /// does.
    pub fn adopt(&mut self, master: SecretBytes) -> Result<()> {
        if self.master.is_some() {
            return Err(Error::AlreadyUnsealed);
        }
        self.master = Some(master);
        Ok(())
    }

    /// Seal the store.
    ///
    /// Dropping the key erases it — [`SecretBytes`] zeroizes on drop — so this
    /// is not a flag being cleared while the bytes stay in the process image.
    pub fn seal(&mut self) {
        self.master = None;
    }

    /// The master key, or a refusal naming the sealed state.
    ///
    /// This is the *open* refusal. A caller who may not address the vault at all
    /// never reaches here: that is the *reach* refusal, decided by grants one
    /// layer up, and the two are separate so that neither can be satisfied by
    /// the other's mechanism.
    pub fn master(&self) -> Result<&SecretBytes> {
        self.master.as_ref().ok_or(Error::Sealed)
    }
}
