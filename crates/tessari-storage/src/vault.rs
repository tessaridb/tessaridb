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
//!
//! # An unseal lasts a period
//!
//! The key is held to a deadline set when it arrives — ten minutes unless the
//! node was told otherwise (ADR-0092 D4). Every use judges the deadline, so a
//! key past it opens nothing even if nobody has dropped it yet; the housekeeping
//! pass drops it so it does not sit in memory until the next use asks.

use std::sync::RwLock;
use std::time::{Duration, Instant, SystemTime};

use tessari_vault::{Keyring, Root, SecretBytes};

use crate::error::{Error, Result};

/// Whether this process can open anything right now, and until when.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SealState {
    /// No master key is held.
    Sealed,
    /// A master key is held and will be dropped at `seals_at`.
    Unsealed {
        /// The wall-clock instant the key stops opening anything.
        seals_at: SystemTime,
    },
}

/// The keyring and the deadline it holds its key to, behind one lock so the two
/// can never be read apart.
#[derive(Debug)]
struct Held {
    keyring: Keyring,
    /// How long the next unseal lasts.
    period: Duration,
    /// When the key held now stops opening anything: monotonic for the judging,
    /// wall-clock for the answer.
    until: Option<(Instant, SystemTime)>,
}

impl Default for Held {
    fn default() -> Self {
        Self {
            keyring: Keyring::sealed(),
            period: Duration::from_secs(tessari_constants::UNSEAL_SECONDS),
            until: None,
        }
    }
}

impl Held {
    /// Whether the key held now is past its deadline.
    fn due(&self) -> bool {
        self.until
            .is_some_and(|(deadline, _)| Instant::now() >= deadline)
    }

    /// Start the period for a key that has just arrived.
    ///
    /// A period too long for the clock to represent ends at once rather than
    /// never: the failure of a deadline is to seal, not to stay open.
    fn start(&mut self) {
        let now = Instant::now();
        let deadline = now.checked_add(self.period).unwrap_or(now);
        let wall = SystemTime::now()
            .checked_add(deadline.duration_since(now))
            .unwrap_or_else(SystemTime::now);
        self.until = Some((deadline, wall));
    }

    fn seal(&mut self) {
        self.keyring.seal();
        self.until = None;
    }
}
/// This process's view of whether the store is open.
///
/// `Default` is sealed, which is the safe direction: a keyring that turns up
/// somewhere by default cannot accidentally be an unsealed one.
#[derive(Default, Debug)]
pub struct OpenVault {
    /// A `RwLock`: sealing and revealing read the keyring on every use; it is
    /// written only by `unseal`, `adopt`, `seal` and a key found past its period.
    held: RwLock<Held>,
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
        self.held
            .read()
            .map_or(true, |held| held.keyring.is_sealed() || held.due())
    }

    /// Whether this process can open anything, and until when.
    #[must_use]
    pub fn state(&self) -> SealState {
        match self.held.read() {
            Ok(held) if !held.keyring.is_sealed() && !held.due() => held
                .until
                .map_or(SealState::Sealed, |(_, seals_at)| SealState::Unsealed {
                    seals_at,
                }),
            _ => SealState::Sealed,
        }
    }

    /// How long an unseal lasts on this process.
    #[must_use]
    pub fn period(&self) -> Duration {
        self.held.read().map_or(Duration::ZERO, |held| held.period)
    }

    /// Set how long every later unseal lasts (ADR-0092 D4).
    ///
    /// The key held now keeps the deadline it was given: a period is a promise
    /// made at the unseal, and changing it underneath would extend a window the
    /// operator already closed in their head.
    pub fn last_for(&self, period: Duration) {
        if let Ok(mut held) = self.held.write() {
            held.period = period;
        }
    }

    /// Drop the key if its period is over, and say whether it did.
    ///
    /// The hygiene half of an expiring unseal: no statement is served by a key
    /// past its deadline whether this runs or not, because every use judges the
    /// deadline itself. This only stops the key sitting in memory until then.
    ///
    /// # Errors
    ///
    /// Returns [`Error::VaultUnavailable`] when the lock is poisoned.
    pub fn seal_if_due(&self) -> Result<bool> {
        let mut held = self.held.write().map_err(|_| Error::VaultUnavailable)?;
        if held.due() {
            held.seal();
            return Ok(true);
        }
        Ok(false)
    }

    /// Unseal with a passphrase, against the store's root record.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Vault`] when the passphrase does not open the record, or
    /// when the store is already unsealed.
    pub fn unseal(&self, root: &Root, passphrase: &str) -> Result<()> {
        let mut held = self.held.write().map_err(|_| Error::VaultUnavailable)?;
        if held.due() {
            held.seal();
        }
        held.keyring
            .unseal(root, passphrase)
            .map_err(Error::Vault)?;
        held.start();
        Ok(())
    }

    /// Adopt a master key produced by initialising the store.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Vault`] when the store is already unsealed.
    pub fn adopt(&self, master: SecretBytes) -> Result<()> {
        let mut held = self.held.write().map_err(|_| Error::VaultUnavailable)?;
        if held.due() {
            held.seal();
        }
        held.keyring.adopt(master).map_err(Error::Vault)?;
        held.start();
        Ok(())
    }

    /// Seal the store.
    ///
    /// # Errors
    ///
    /// Returns [`Error::VaultUnavailable`] when the lock is poisoned.
    pub fn seal(&self) -> Result<()> {
        let mut held = self.held.write().map_err(|_| Error::VaultUnavailable)?;
        held.seal();
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
        {
            let held = self.held.read().map_err(|_| Error::VaultUnavailable)?;
            if !held.due() {
                let master = held.keyring.master().map_err(Error::Vault)?;
                return act(master);
            }
        }
        // Past its period: drop it now rather than leave that to the
        // housekeeping pass, and answer as the sealed store this is.
        self.seal_if_due()?;
        Err(Error::Vault(tessari_vault::Error::Sealed))
    }
}
