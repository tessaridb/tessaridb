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

use tessari_ql::{InfoSubject, Script, Span, Statement, StatementKind};
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

impl Session<'_> {
    /// Carry out `act` and answer as `INFO FOR SEAL` does, plus
    /// `initialised: true` on the unseal that created the root.
    ///
    /// # Errors
    ///
    /// Whatever the matching statement is refused with — the authority it
    /// needs, a wrong passphrase, the throttle.
    pub fn vault(&mut self, act: VaultAct<'_>) -> Result<Value> {
        let span = Span::new(0, 0);
        let kind = match act {
            VaultAct::Status => None,
            VaultAct::Unseal { passphrase } => Some(StatementKind::UnsealVault {
                passphrase: passphrase.to_owned(),
                span,
            }),
            VaultAct::Seal => Some(StatementKind::SealVault { span }),
            VaultAct::Change { current, new } => Some(StatementKind::ChangeVaultPassphrase {
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
                subject: InfoSubject::Seal,
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
