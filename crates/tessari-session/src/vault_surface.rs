//! The vault's dedicated surface: unseal, seal and ask, with no statement text
//! (ADR-0092 D2).
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

use tessari_ql::{InfoSubject, Name, Script, Span, Statement, StatementKind};
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
    pub fn vault(&mut self, target: VaultTarget<'_>, act: VaultAct<'_>) -> Result<Value> {
        match target {
            VaultTarget::Store => self.vault_act(None, act),
            VaultTarget::Vault {
                namespace,
                database,
                vault,
            } => {
                let held = (
                    self.namespace.replace(namespace.to_owned()),
                    self.database.replace(database.to_owned()),
                );
                let answered = self.vault_act(Some(vault), act);
                (self.namespace, self.database) = held;
                answered
            }
        }
    }

    fn vault_act(&mut self, vault: Option<&str>, act: VaultAct<'_>) -> Result<Value> {
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
                span,
            }),
        };
        let initialised = match kind {
            Some(kind) => matches!(
                self.built(kind)?.as_slice(),
                [Outcome::Value(Value::String(said))] if said == "initialised"
            ),
            None => false,
        };
        let mut report = match self
            .built(StatementKind::Info {
                subject: InfoSubject::Seal(vault),
            })?
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
    fn built(&mut self, kind: StatementKind) -> Result<Vec<Outcome>> {
        let span = Span::new(0, 0);
        self.run_script(Script {
            statements: vec![Statement { kind, span }],
            span,
        })
    }
}
