//! A table's expiry declaration: `DEFINE … EXPIRE [AFTER d]`, `ALTER TABLE …
//! SET EXPIRE [AFTER d]` and `DROP EXPIRE` (ADR-0122 A1, A4–A6, A8).
//!
//! Every statement here changes the declaration and **no stored record**: a
//! record already in the table keeps whatever instant it carries, so turning
//! expiry on never expires what is there, a shorter default never expires it in
//! bulk, and retiring the declaration never brings back what users were told
//! was gone. The answer to an `ALTER` says so, because it is the one fact an
//! operator changing a lifetime on a populated table most needs to read.

use std::collections::BTreeMap;

use tessari_encoding::FormatVersion;
use tessari_ql::{Name, Span, TableExpiry, TableRef, WriteExpiry};
use tessari_storage::{Catalog, RecordAddress, TableKind, Transaction};
use tessari_types::{TableId, Value};

use crate::error::{Error, Result};
use crate::kv::expiry::Instant;
use crate::outcome::Outcome;
use crate::session::Session;

/// What an `ALTER … SET EXPIRE` says about the records already stored.
const UNCHANGED: &str = "unchanged";

/// What an `ALTER … DROP EXPIRE` says about them.
const STILL_EXPIRE: &str = "still expire at their instants";

impl Session<'_> {
    /// Refuse an expiry declaration the table's kind or the store's format
    /// cannot carry, before anything is written.
    pub(super) fn refuse_an_expiry_it_cannot_carry(
        &self,
        table: &str,
        kind: &TableKind,
        span: Span,
    ) -> Result<()> {
        if !matches!(kind, TableKind::Table | TableKind::Collection) {
            return Err(Error::ExpiryNotOnThisKind {
                table: table.to_owned(),
                kind: kind_word(kind),
                span,
            });
        }
        self.refuse_a_format_the_store_does_not_hold(
            "a table whose records expire",
            FormatVersion::TABLE_EXPIRY,
        )
    }

    /// Whether the session's database already holds a table by this name — the
    /// one fact `IF NOT EXISTS` turns on, asked before the definition so an
    /// existing table's declaration is never rewritten by a `DEFINE`.
    pub(super) fn names_a_table(&self, transaction: &mut Transaction<'_>, name: &Name) -> bool {
        self.resolve_any_table(transaction, &table_named(name))
            .is_ok()
    }

    /// Write the declaration a `DEFINE` carried onto the table it created.
    pub(super) fn declare_expiry_on(
        &self,
        transaction: &mut Transaction<'_>,
        name: &Name,
        expire: TableExpiry,
    ) -> Result<()> {
        let (_, table) = self.resolve_any_table(transaction, &table_named(name))?;
        Catalog::new(transaction).set_expiry(table, stored(expire))?;
        Ok(())
    }

    /// `ALTER TABLE … SET EXPIRE [AFTER d]` and `… DROP EXPIRE`.
    pub(super) fn alter_expiry(
        &self,
        transaction: &mut Transaction<'_>,
        table: TableId,
        name: &str,
        change: Option<TableExpiry>,
        span: Span,
    ) -> Result<Outcome> {
        // Resolved by name a moment ago in this transaction, so there is a
        // definition; one that vanished in between has nothing to declare.
        let Some(definition) = Catalog::new(transaction).table(table)? else {
            return Ok(Outcome::Done);
        };
        match change {
            Some(_) => self.refuse_an_expiry_it_cannot_carry(name, &definition.kind, span)?,
            // Nothing to retire on a table that never declared it.
            None if definition.expire.is_none() => {
                return Err(Error::TableDoesNotExpire {
                    table: name.to_owned(),
                    span,
                });
            }
            None => {}
        }
        let (declared, existing) = match change {
            Some(expire) => (stored(expire), UNCHANGED),
            // Retiring keeps the lifetime it had, so `INFO` can still say what
            // the table used to give a new record.
            None => (
                tessari_storage::TableExpiry {
                    after: definition.expire.and_then(|held| held.after),
                    retired: true,
                },
                STILL_EXPIRE,
            ),
        };
        Catalog::new(transaction).set_expiry(table, declared)?;
        Ok(Outcome::Value(Value::Object(BTreeMap::from([
            ("expire".to_owned(), crate::info::described_expiry(declared)),
            ("existing".to_owned(), Value::from(existing)),
        ]))))
    }
}

/// A reference to a table in the session's own database, by the name a `DEFINE`
/// gave it.
fn table_named(name: &Name) -> TableRef {
    TableRef {
        database: None,
        name: name.clone(),
        span: name.span,
    }
}

/// The declaration as the catalog stores it.
fn stored(expire: TableExpiry) -> tessari_storage::TableExpiry {
    tessari_storage::TableExpiry {
        after: expire.after,
        retired: false,
    }
}

/// A write's `EXPIRE` clause, resolved before anything is written.
#[derive(Debug, Clone, Copy)]
pub(super) enum Settled {
    /// The record stops being answered at this millisecond.
    At(u64),
    /// The record never expires.
    Never,
}

impl Session<'_> {
    /// Resolve a write's `EXPIRE` clause against its table and the
    /// transaction's clock, refusing before anything is written (ADR-0122 A2).
    pub(super) fn resolve_write_expiry(
        &self,
        transaction: &mut Transaction<'_>,
        table: TableId,
        expire: Option<&WriteExpiry>,
    ) -> Result<Option<Settled>> {
        let when = match expire {
            None => return Ok(None),
            Some(WriteExpiry::Never(_)) => return Ok(Some(Settled::Never)),
            Some(WriteExpiry::At(when)) => when,
        };
        let definition = Catalog::new(transaction).table(table)?;
        if !definition
            .as_ref()
            .and_then(|held| held.expire)
            .is_some_and(|expire| !expire.retired)
        {
            return Err(Error::TableDoesNotExpire {
                table: definition.map(|held| held.name).unwrap_or_default(),
                span: when.span,
            });
        }
        match self.instant(transaction, when)? {
            Instant::Future(at) => Ok(Some(Settled::At(at))),
            Instant::Passed => Err(Error::InvalidExpiry {
                reason: "a write's EXPIRE must be in the future",
                span: when.span,
            }),
        }
    }
}

/// Apply a resolved clause to the write just buffered for `address`.
pub(super) fn settle_write_expiry(
    transaction: &mut Transaction<'_>,
    address: &RecordAddress,
    settled: Option<Settled>,
) {
    match settled {
        Some(Settled::At(at)) => transaction.expire_pending(address, at),
        Some(Settled::Never) => transaction.persist_pending(address),
        None => {}
    }
}

/// The word that declared a kind of table, for a refusal to name it by.
fn kind_word(kind: &TableKind) -> &'static str {
    match kind {
        TableKind::Table => "table",
        TableKind::Collection => "collection",
        TableKind::Bucket(_) => "bucket",
        TableKind::Edge(_) => "edge table",
        TableKind::Vector(_) => "vector store",
        TableKind::Geo => "geo store",
        TableKind::Vault(_) => "vault",
        TableKind::Queue(_) => "queue",
        TableKind::View(_) => "view",
        TableKind::Series(_) => "series",
        TableKind::Space(_) => "space",
        TableKind::Topic(_) => "topic",
    }
}
