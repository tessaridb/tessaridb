//! The keys of vaults that carry their own passphrase (ADR-0093), held beside
//! the store's master key and to the same rules: memory only, one period each,
//! judged at every use, and gone with the process.

use std::collections::HashMap;
use std::time::{Duration, Instant, SystemTime};

use tessari_vault::{KeyId, Level, Root, SecretBytes, keys};

use super::{OpenVault, SealState};
use crate::catalog::VaultCustody;
use crate::error::{Error, Result};

/// One vault's key and the moment it stops opening anything.
#[derive(Debug)]
struct Held {
    key: SecretBytes,
    /// Monotonic for the judging, wall-clock for the answer.
    until: (Instant, SystemTime),
}

impl Held {
    fn due(&self) -> bool {
        Instant::now() >= self.until.0
    }
}

/// Every own-custody vault key this process holds, by the key's identifier.
///
/// Keyed by the identifier rather than by name so a vault dropped and declared
/// again under the same name is a different entry: the old key cannot be
/// mistaken for the new vault's, and simply lapses.
#[derive(Debug, Default)]
pub(super) struct OwnKeys(HashMap<KeyId, Held>);

impl OwnKeys {
    /// Whether `key_id` is held and inside its period.
    pub(super) fn is_open(&self, key_id: KeyId) -> bool {
        self.0.get(&key_id).is_some_and(|held| !held.due())
    }

    pub(super) fn state(&self, key_id: KeyId) -> SealState {
        match self.0.get(&key_id) {
            Some(held) if !held.due() => SealState::Unsealed {
                seals_at: held.until.1,
            },
            _ => SealState::Sealed,
        }
    }

    /// Hold `key` for `period`, starting now.
    ///
    /// A period too long for the clock to represent ends at once rather than
    /// never, as the store's does.
    pub(super) fn hold(&mut self, key_id: KeyId, key: SecretBytes, period: Duration) {
        let now = Instant::now();
        let deadline = now.checked_add(period).unwrap_or(now);
        let wall = SystemTime::now()
            .checked_add(deadline.duration_since(now))
            .unwrap_or_else(SystemTime::now);
        self.0.insert(
            key_id,
            Held {
                key,
                until: (deadline, wall),
            },
        );
    }

    pub(super) fn seal(&mut self, key_id: KeyId) {
        self.0.remove(&key_id);
    }

    /// The key for `key_id`, when it is held and inside its period.
    pub(super) fn key(&self, key_id: KeyId) -> Option<&SecretBytes> {
        self.0
            .get(&key_id)
            .filter(|held| !held.due())
            .map(|held| &held.key)
    }

    /// Drop every key past its period, and say whether any went.
    pub(super) fn drop_due(&mut self) -> bool {
        let before = self.0.len();
        self.0.retain(|_, held| !held.due());
        self.0.len() != before
    }
}

/// Vaults carrying their own passphrase (ADR-0093): the same period, the same
/// judging at every use, one deadline per vault key.
impl OpenVault {
    /// Unseal one vault with its own passphrase, against its own root.
    ///
    /// The derivation runs outside the lock: it is the slow part by design, and
    /// every read of every vault waits on this lock.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Vault`] carrying `AlreadyUnsealed` when the vault's key
    /// is held and inside its period, and `WrongKey` when the passphrase does
    /// not open the root.
    pub fn unseal_own(&self, root: &Root, scope: &[u8], passphrase: &str) -> Result<()> {
        let already = || Error::Vault(tessari_vault::Error::AlreadyUnsealed);
        if self
            .held
            .read()
            .map_err(|_| Error::VaultUnavailable)?
            .own
            .is_open(root.key_id)
        {
            return Err(already());
        }
        let key = root.unlock_vault(passphrase, scope).map_err(Error::Vault)?;
        let mut held = self.held.write().map_err(|_| Error::VaultUnavailable)?;
        if held.own.is_open(root.key_id) {
            return Err(already());
        }
        let period = held.period;
        held.own.hold(root.key_id, key, period);
        Ok(())
    }

    /// Hold a vault key just minted by declaring the vault, for one period.
    ///
    /// # Errors
    ///
    /// Returns [`Error::VaultUnavailable`] when the lock is poisoned.
    pub fn adopt_own(&self, key_id: KeyId, key: SecretBytes) -> Result<()> {
        let mut held = self.held.write().map_err(|_| Error::VaultUnavailable)?;
        let period = held.period;
        held.own.hold(key_id, key, period);
        Ok(())
    }

    /// Drop one vault's own key.
    ///
    /// # Errors
    ///
    /// Returns [`Error::VaultUnavailable`] when the lock is poisoned.
    pub fn seal_own(&self, key_id: KeyId) -> Result<()> {
        let mut held = self.held.write().map_err(|_| Error::VaultUnavailable)?;
        held.own.seal(key_id);
        Ok(())
    }

    /// Whether one vault's own key is held, and until when.
    ///
    /// A poisoned lock reads as sealed, as [`Self::state`] does.
    #[must_use]
    pub fn own_state(&self, key_id: KeyId) -> SealState {
        self.held
            .read()
            .map_or(SealState::Sealed, |held| held.own.state(key_id))
    }

    /// Do something with a vault's key, opened from whichever custody holds it.
    ///
    /// The one place a vault key is opened, so the two custodies cannot drift
    /// apart at the two call sites that need one. The key exists for the length
    /// of the call.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Vault`] carrying `Sealed` when the key that opens this
    /// vault is not held — the master key for a store-custody vault, the
    /// vault's own for an own-custody one — and whatever the closure returns
    /// otherwise.
    pub fn with_vault_key<T>(
        &self,
        custody: &VaultCustody,
        scope: &[u8],
        act: impl FnOnce(&SecretBytes) -> Result<T>,
    ) -> Result<T> {
        match custody {
            VaultCustody::Store(wrapped) => self.with_master(|master| {
                let vault_key = keys::unwrap(master, Level::Vault, scope, wrapped)?;
                act(&vault_key)
            }),
            VaultCustody::Own(root) => {
                {
                    let held = self.held.read().map_err(|_| Error::VaultUnavailable)?;
                    if let Some(vault_key) = held.own.key(root.key_id) {
                        return act(vault_key);
                    }
                }
                self.seal_if_due()?;
                Err(Error::Vault(tessari_vault::Error::Sealed))
            }
        }
    }
}
