//! Events: statements a table runs after each write of one of its records, in
//! the writer's transaction, as the writer (ADR-0110).
//!
//! # Where they run
//!
//! In the record-write funnel — [`Session::put_record`]'s sealing path and
//! [`Session::delete_record`] — because every caller-driven write passes through
//! there and nothing else does: a statement, a script, a client, a Kafka or
//! topic consumer. The engine's own writes (rollup rows, kept views, claims,
//! expiry) take other paths and never run one, and neither does replication or
//! a restore, which apply commits whose event effects are already in them.
//!
//! # As whom
//!
//! A child session: the writer's identity, the event table's namespace and
//! database, one level deeper. Each statement of the body is authorized exactly
//! as the writer's own would be, and `$before`/`$after` are redacted to the
//! fields the writer may read — a body could otherwise copy a field the writer
//! cannot see into a table the writer can.

mod fire;

use tessari_ql::{Name, Span, TableRef};
use tessari_storage::{Catalog, EventDeclaration, TableKind, Transaction};
use tessari_types::{Value, WriteKind};

use crate::error::{Error, Result};
use crate::outcome::Outcome;
use crate::session::Session;

/// The events a write must run once it has landed, and what it replaced.
pub(crate) struct Pending {
    /// The table's name, for `$id`'s refusals and `EventFailed`.
    pub(super) table: String,
    /// Its events, in name order.
    pub(super) events: Vec<EventDeclaration>,
    /// The record before the write, as stored.
    pub(super) old: Option<Value>,
}

/// A `DEFINE EVENT` statement's parts, as the parser left them.
pub(crate) struct Declared<'s> {
    pub(crate) name: &'s Name,
    pub(crate) table: &'s TableRef,
    pub(crate) on: &'s [WriteKind],
    pub(crate) when: Option<&'s String>,
    pub(crate) body: &'s str,
    pub(crate) if_not_exists: bool,
}

/// The word a refused kind is named by.
const fn kind_word(kind: &TableKind) -> Option<&'static str> {
    match kind {
        TableKind::Table | TableKind::Collection | TableKind::Edge(_) => None,
        TableKind::Bucket(_) => Some("bucket"),
        TableKind::Vector(_) => Some("vector store"),
        TableKind::Geo => Some("geo store"),
        TableKind::Vault(_) => Some("vault"),
        TableKind::Queue(_) => Some("queue"),
        TableKind::View(_) => Some("view"),
        TableKind::Series(_) => Some("series"),
        TableKind::Space(_) => Some("space"),
        TableKind::Topic(_) => Some("topic"),
    }
}

impl<'a> Session<'a> {
    /// `DEFINE EVENT` (ADR-0110 D1, D9, D10).
    ///
    /// # Errors
    ///
    /// [`Error::EventOnKind`] on a table that cannot carry one,
    /// [`Error::EventExists`] for a name already defined there, and whatever
    /// resolving the table refuses.
    pub(crate) fn define_event(
        &self,
        transaction: &mut Transaction<'_>,
        declared: &Declared<'_>,
        span: Span,
    ) -> Result<Outcome> {
        let (_, id) = self.resolve_any_table(transaction, declared.table)?;
        let Some(definition) = Catalog::new(transaction).table(id)? else {
            return Err(Error::Unknown {
                entity: "table",
                name: declared.table.name.text.clone(),
                span: declared.table.span,
            });
        };
        if let Some(kind) = kind_word(&definition.kind) {
            return Err(Error::EventOnKind {
                table: definition.name,
                kind,
                span: declared.table.span,
            });
        }
        // A write straddling this definition retries and sees it, the rule
        // `DEFINE ROLLUP` keeps (ADR-0110 D10).
        transaction.guard_table_entry(id);
        let mut events = definition.events;
        if events.iter().any(|held| held.name == declared.name.text) {
            if declared.if_not_exists {
                return Ok(Outcome::Done);
            }
            return Err(Error::EventExists {
                event: declared.name.text.clone(),
                table: definition.name,
                span,
            });
        }
        events.push(EventDeclaration {
            name: declared.name.text.clone(),
            on: declared.on.to_vec(),
            when: declared.when.cloned(),
            body: declared.body.to_owned(),
        });
        events.sort_by(|left, right| left.name.cmp(&right.name));
        Catalog::new(transaction).set_events(id, events)?;
        Ok(Outcome::Done)
    }

    /// `DROP EVENT`.
    ///
    /// # Errors
    ///
    /// [`Error::Unknown`] naming the event when the table has none by that
    /// name, and whatever resolving the table refuses.
    pub(crate) fn drop_event(
        &self,
        transaction: &mut Transaction<'_>,
        name: &Name,
        table: &TableRef,
    ) -> Result<Outcome> {
        let (_, id) = self.resolve_any_table(transaction, table)?;
        let Some(definition) = Catalog::new(transaction).table(id)? else {
            return Err(Error::Unknown {
                entity: "table",
                name: table.name.text.clone(),
                span: table.span,
            });
        };
        transaction.guard_table_entry(id);
        let mut events = definition.events;
        let before = events.len();
        events.retain(|held| held.name != name.text);
        if events.len() == before {
            return Err(Error::Unknown {
                entity: "event",
                name: name.text.clone(),
                span: name.span,
            });
        }
        Catalog::new(transaction).set_events(id, events)?;
        Ok(Outcome::Done)
    }
}
