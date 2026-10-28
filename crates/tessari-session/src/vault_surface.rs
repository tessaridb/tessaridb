//! The vault's dedicated surface: unseal, seal, ask and declare, with no
//! statement text (ADR-0092 D2).
//!
//! # Why the acts are statements built rather than read
//!
//! A passphrase written into a script is text, and text is what a console keeps
//! in its history, what a client logs when a request fails, and what a proxy
//! records. The routes and the wire frame carry it as its own field instead —
//! and then hand it here, where it becomes the same statement `UNSEAL VAULT WITH`
//! parses to, without ever having been spelled. Running that statement through
//! the ordinary path is the point: the authority it demands, the throttle and the
//! outcome are the statement's own, so the surface cannot drift from the
//! language by being a second implementation of it.

use std::collections::BTreeMap;

use tessari_ql::{InfoSubject, Name, Script, Span, Statement, StatementKind, VaultKey};
use tessari_storage::Presented;
use tessari_types::Value;

use crate::error::Result;
use crate::outcome::Outcome;
use crate::session::Session;

/// One act on the vault, as a dedicated surface asks for it.
#[derive(Debug, Clone, Copy)]
pub enum VaultAct<'a> {
    /// Whether this process can open secrets, and until when.
    Status,
    /// Present the passphrase; the first one on a store initialises its root.
    Unseal {
        /// The passphrase. Never formatted, logged or echoed.
        passphrase: &'a str,
    },
    /// Drop the master key.
    Seal,
    /// Wrap the master key under a new passphrase (ADR-0092 D3).
    Change {
        /// The passphrase that opens the store now.
        current: &'a str,
        /// The passphrase that will open it afterwards.
        new: &'a str,
    },
}

/// One key a request presents for a vault in keyed custody (ADR-0136). Cleared when dropped.
pub struct PresentedKey {
    /// The namespace the vault lives in.
    pub namespace: String,
    /// The database it lives in.
    pub database: String,
    /// The vault's name.
    pub vault: String,
    /// The key.
    pub key: [u8; 32],
}

impl Drop for PresentedKey {
    fn drop(&mut self) {
        zeroize::Zeroize::zeroize(&mut self.key);
    }
}

impl std::fmt::Debug for PresentedKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "PresentedKey({}.{}.{})",
            self.namespace, self.database, self.vault
        )
    }
}

/// What an act is about: the store's own key, or one vault's (ADR-0093 D6).
#[derive(Debug, Clone, Copy)]
pub enum VaultTarget<'a> {
    /// The store's master key and every vault in its custody.
    Store,
    /// One vault, by its tenancy and name.
    Vault {
        /// The namespace it lives in.
        namespace: &'a str,
        /// The database it lives in.
        database: &'a str,
        /// Its name.
        vault: &'a str,
    },
}

impl Session<'_> {
    /// Carry out `act` on `target` and answer as `INFO FOR SEAL` (or
    /// `INFO FOR SEAL OF`) does, plus `initialised: true` on the unseal that
    /// created the store's root.
    ///
    /// A vault target runs in its own tenancy and leaves the session's `USE`
    /// as it found it, because the connection it arrived on is somebody's
    /// session and a surface act is not a `USE`.
    ///
    /// # Errors
    ///
    /// Whatever the matching statement is refused with — the authority it
    /// needs, a wrong passphrase, the throttle, a vault in the store's custody.
    pub async fn vault(&mut self, target: VaultTarget<'_>, act: VaultAct<'_>) -> Result<Value> {
        match target {
            VaultTarget::Store => self.vault_act(None, act).await,
            VaultTarget::Vault {
                namespace,
                database,
                vault,
            } => {
                let held = (
                    self.namespace.replace(namespace.to_owned()),
                    self.database.replace(database.to_owned()),
                );
                let answered = self.vault_act(Some(vault), act).await;
                (self.namespace, self.database) = held;
                answered
            }
        }
    }

    /// Declare `vault` in `namespace`.`database` keyed by its own `passphrase`
    /// (ADR-0093), and answer as `INFO FOR SEAL OF` does.
    ///
    /// The statement `DEFINE VAULT … PASSPHRASE '…'` parses to, built here from
    /// a field so the passphrase is never statement text (DB-22), and run as the
    /// caller in the vault's tenancy with the session's `USE` left as it was.
    ///
    /// # Errors
    ///
    /// Whatever `DEFINE VAULT` is refused with — the authority it needs, a name
    /// already taken, a tenancy that does not exist.
    pub async fn declare_vault(
        &mut self,
        namespace: &str,
        database: &str,
        vault: &str,
        passphrase: &str,
    ) -> Result<Value> {
        let held = (
            self.namespace.replace(namespace.to_owned()),
            self.database.replace(database.to_owned()),
        );
        let span = Span::new(0, 0);
        let declared = match self
            .built(StatementKind::DefineVault {
                name: Name {
                    text: vault.to_owned(),
                    span,
                },
                if_not_exists: false,
                passphrase: Some(passphrase.to_owned()),
                key: None,
            })
            .await
        {
            Ok(_) => self.vault_act(Some(vault), VaultAct::Status).await,
            Err(refused) => Err(refused),
        };
        (self.namespace, self.database) = held;
        declared
    }

    /// Declare `vault` in `namespace`.`database` in keyed custody (ADR-0136): its key is wrapped under `key`, which
    /// the caller presents again with every request that opens it. Answers as `INFO FOR SEAL OF` does.
    ///
    /// # Errors
    ///
    /// Whatever `DEFINE VAULT` is refused with.
    pub async fn declare_keyed_vault(
        &mut self,
        namespace: &str,
        database: &str,
        vault: &str,
        key: [u8; 32],
    ) -> Result<Value> {
        let held = (
            self.namespace.replace(namespace.to_owned()),
            self.database.replace(database.to_owned()),
        );
        let span = Span::new(0, 0);
        let declared = match self
            .built(StatementKind::DefineVault {
                name: Name {
                    text: vault.to_owned(),
                    span,
                },
                if_not_exists: false,
                passphrase: None,
                key: Some(VaultKey::new(key)),
            })
            .await
        {
            Ok(_) => self.vault_act(Some(vault), VaultAct::Status).await,
            Err(refused) => Err(refused),
        };
        (self.namespace, self.database) = held;
        declared
    }

    /// Re-wrap a keyed vault's key under `new`, proving `current` (ADR-0136).
    ///
    /// # Errors
    ///
    /// `VaultNotKeyed`, a wrong `current`, the throttle, the authority `CHANGE` needs.
    pub async fn change_vault_key(
        &mut self,
        namespace: &str,
        database: &str,
        vault: &str,
        current: [u8; 32],
        new: [u8; 32],
    ) -> Result<Value> {
        let held = (
            self.namespace.replace(namespace.to_owned()),
            self.database.replace(database.to_owned()),
        );
        let span = Span::new(0, 0);
        let changed = match self
            .built(StatementKind::ChangeVaultPassphrase {
                vault: Some(Name {
                    text: vault.to_owned(),
                    span,
                }),
                current: String::new(),
                new: String::new(),
                key: Some((VaultKey::new(current), VaultKey::new(new))),
                span,
            })
            .await
        {
            Ok(_) => self.vault_act(Some(vault), VaultAct::Status).await,
            Err(refused) => Err(refused),
        };
        (self.namespace, self.database) = held;
        changed
    }

    /// Check every key a request presents and gather them for [`tessari_storage::presenting`] (ADR-0136).
    ///
    /// Each key must open its vault's root — a wrong one is refused here, under that vault's throttle, before any
    /// statement of the request runs — and each vault must be in keyed custody. The session's `USE` is left as it
    /// was.
    ///
    /// # Errors
    ///
    /// `Unknown` for a vault that is not there, `VaultNotKeyed`, `WrongKey`, the throttle, a tenancy refusal.
    pub async fn presented_keys(&mut self, keys: &[PresentedKey]) -> Result<Presented> {
        let mut presented = Presented::none();
        for asked in keys {
            let held = (
                self.namespace.replace(asked.namespace.clone()),
                self.database.replace(asked.database.clone()),
            );
            let key = tessari_vault::SecretBytes::adopt(asked.key);
            let checked = self.checked_key(&asked.vault, &key).await;
            (self.namespace, self.database) = held;
            presented.insert(checked?, key);
        }
        Ok(presented)
    }

    async fn checked_key(
        &mut self,
        vault: &str,
        key: &tessari_vault::SecretBytes,
    ) -> Result<Vec<u8>> {
        let span = Span::new(0, 0);
        let name = Name {
            text: vault.to_owned(),
            span,
        };
        let mut transaction = self.store.begin().await?;
        let checked = self
            .check_presented(&mut transaction, &name, key, span)
            .await;
        transaction.rollback();
        checked
    }

    async fn vault_act(&mut self, vault: Option<&str>, act: VaultAct<'_>) -> Result<Value> {
        let span = Span::new(0, 0);
        let vault = vault.map(|vault| Name {
            text: vault.to_owned(),
            span,
        });
        let kind = match act {
            VaultAct::Status => None,
            VaultAct::Unseal { passphrase } => Some(StatementKind::UnsealVault {
                vault: vault.clone(),
                passphrase: passphrase.to_owned(),
                span,
            }),
            VaultAct::Seal => Some(StatementKind::SealVault {
                vault: vault.clone(),
                span,
            }),
            VaultAct::Change { current, new } => Some(StatementKind::ChangeVaultPassphrase {
                vault: vault.clone(),
                current: current.to_owned(),
                new: new.to_owned(),
                key: None,
                span,
            }),
        };
        let initialised = match kind {
            Some(kind) => matches!(
                self.built(kind).await?.as_slice(),
                [Outcome::Value(Value::String(said))] if said == "initialised"
            ),
            None => false,
        };
        let mut report = match self
            .built(StatementKind::Info {
                subject: InfoSubject::Seal(vault),
            })
            .await?
            .pop()
        {
            Some(Outcome::Value(Value::Object(report))) => report,
            _ => BTreeMap::new(),
        };
        if initialised {
            report.insert("initialised".to_owned(), Value::Bool(true));
        }
        Ok(Value::Object(report))
    }

    /// Run one statement that was built rather than read.
    async fn built(&mut self, kind: StatementKind) -> Result<Vec<Outcome>> {
        let span = Span::new(0, 0);
        self.run_script(Script {
            statements: vec![Statement {
                kind,
                span,
                acknowledge: None,
                across: false,
            }],
            span,
        })
        .await
    }
}
