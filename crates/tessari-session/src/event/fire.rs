//! Running a table's events after a write (ADR-0110 D2, D4, D5, D6).

use std::collections::BTreeMap;

use tessari_constants::EVENT_DEPTH_LIMIT;
use tessari_encoding::decode_payload;
use tessari_ql::{
    Identity, Name, Parameters, RecordTarget, Script, Span, Statement, StatementKind, TableRef,
};
use tessari_storage::{Catalog, EventDeclaration, RecordAddress, Transaction};
use tessari_types::{Value, WriteKind};

use super::Pending;
use crate::error::{Error, Result};
use crate::outcome::Outcome;
use crate::session::Session;

impl<'a> Session<'a> {
    /// Before a caller writes or deletes `address`: the table's events, and the
    /// record as it stands, when the table has any (ADR-0110 D10).
    ///
    /// # Errors
    ///
    /// Whatever reading the catalog or the record refuses.
    pub(crate) fn events_before(
        &self,
        transaction: &mut Transaction<'_>,
        address: &RecordAddress,
    ) -> Result<Option<Pending>> {
        let Some(definition) = Catalog::new(transaction).table(address.table)? else {
            return Ok(None);
        };
        if definition.events.is_empty() {
            return Ok(None);
        }
        let old = match transaction.get(address)? {
            Some(bytes) => Some(decode_payload(&bytes).map_err(tessari_storage::Error::from)?),
            None => None,
        };
        Ok(Some(Pending {
            table: definition.name,
            events: definition.events,
            old,
        }))
    }

    /// After the write or delete landed: run every event the write calls for,
    /// in name order (ADR-0110 D2, D4, D5).
    ///
    /// # Errors
    ///
    /// [`Error::EventFailed`] carrying the body's refusal, and
    /// [`Error::EventDepth`] past the limit.
    pub(crate) fn events_after(
        &self,
        transaction: &mut Transaction<'_>,
        address: &RecordAddress,
        pending: Pending,
        new: Option<&Value>,
    ) -> Result<()> {
        let kind = match (&pending.old, new) {
            (None, Some(_)) => WriteKind::Create,
            (Some(_), Some(_)) => WriteKind::Update,
            (Some(_), None) => WriteKind::Delete,
            // A record that was not there was not deleted.
            (None, None) => return Ok(()),
        };
        let mut child: Option<(Session<'a>, Parameters)> = None;
        for event in pending.events.iter().filter(|event| event.runs_on(kind)) {
            if self.event_depth >= EVENT_DEPTH_LIMIT {
                return Err(Error::EventDepth {
                    event: event.name.clone(),
                    limit: EVENT_DEPTH_LIMIT,
                });
            }
            if child.is_none() {
                child = Some(self.event_session(transaction, address, &pending, new, kind)?);
            }
            let Some((session, bindings)) = child.as_mut() else {
                continue;
            };
            session
                .run_event(transaction, event, bindings)
                .map_err(|cause| Error::EventFailed {
                    event: event.name.clone(),
                    table: pending.table.clone(),
                    cause: Box::new(cause),
                })?;
        }
        Ok(())
    }

    /// The session a body runs in and the four values it is bound to.
    fn event_session(
        &self,
        transaction: &mut Transaction<'_>,
        address: &RecordAddress,
        pending: &Pending,
        new: Option<&Value>,
        kind: WriteKind,
    ) -> Result<(Session<'a>, Parameters)> {
        let catalog = Catalog::new(transaction);
        let namespace = catalog.namespace(address.namespace)?.map(|held| held.name);
        let database = catalog.database(address.database)?.map(|held| held.name);
        let mut session = Session {
            store: self.store,
            namespace,
            database,
            identity: self.identity.clone(),
            consumer: self.consumer.clone(),
            elsewhere: self.elsewhere.clone(),
            gather: None,
            // An event body runs inside the writer's transaction, which commits
            // across leaders or not as a whole; the body never coordinates.
            participants: None,
            backups: None,
            at_rest: None,
            budget: self.budget.clone(),
            certificates: None,
            sink: crate::backup_to::Sink::none(),
            landed: false,
            acknowledge_open: None,
            event_depth: self.event_depth.saturating_add(1),
        };
        // The record as the writer may read it: a read of it, authorized as the
        // writer's own would be, and then their field grant. A writer who may
        // write the table and not read it sees neither side — the body could
        // otherwise copy what they cannot read somewhere they can.
        let probe = StatementKind::Get {
            target: RecordTarget {
                table: TableRef {
                    database: None,
                    name: Name {
                        text: pending.table.clone(),
                        span: Span::new(0, 0),
                    },
                    span: Span::new(0, 0),
                },
                id: Identity::Fixed(address.id.clone()),
                span: Span::new(0, 0),
            },
        };
        let open = self.identity.user().is_none();
        let readable = session
            .authorize_as_refreshed(self.store, &probe, open, Span::new(0, 0))
            .is_ok();
        let visible = self.visible_in(transaction, address.table)?;
        let seen = |value: Option<&Value>| match value {
            Some(value) if readable => crate::redact::seen(value.clone(), &visible),
            _ => Value::None,
        };
        let bindings = BTreeMap::from([
            ("event".to_owned(), Value::from(kind.word())),
            ("before".to_owned(), seen(pending.old.as_ref())),
            ("after".to_owned(), seen(new)),
            // The identity, the value written after `orders:` — so a body
            // names its own record `orders:$id`, the one place the language
            // already takes a value where a name stands.
            ("id".to_owned(), crate::info::id_value(address.id.clone())),
        ]);
        Ok((session, bindings))
    }

    /// One event's condition and body, in `transaction`, as this session.
    fn run_event(
        &mut self,
        transaction: &mut Transaction<'_>,
        event: &EventDeclaration,
        bindings: &Parameters,
    ) -> Result<()> {
        if let Some(when) = &event.when {
            let condition = Script {
                statements: vec![Statement {
                    kind: StatementKind::Return {
                        value: tessari_ql::parse_expression(when)?,
                    },
                    span: Span::new(0, 0),
                    acknowledge: None,
                }],
                span: Span::new(0, 0),
            }
            .bind(bindings)?;
            let Some(StatementKind::Return { value }) = condition
                .statements
                .first()
                .map(|statement| &statement.kind)
            else {
                return Ok(());
            };
            if self.evaluate(transaction, value)? != Value::Bool(true) {
                return Ok(());
            }
        }
        let mut body = tessari_ql::parse(&event.body)?.bind(bindings)?;
        // A writer that reached a write is either signed in, which means the
        // store has a user and is closed, or anonymous on a store with none.
        let open = self.identity.user().is_none();
        let mut at = 0usize;
        while at < body.statements.len() {
            let statement = &body.statements[at];
            let span = statement.span;
            let expanded = self.expand_views(self.store, &statement.kind)?;
            let kind = expanded.as_ref().unwrap_or(&statement.kind);
            self.authorize_as_refreshed(self.store, kind, open, span)?;
            let outcome = self.execute(transaction, kind, span)?;
            if let StatementKind::Let { name, .. } = &body.statements[at].kind {
                let Outcome::Value(bound) = outcome else {
                    return Err(Error::BindingIsNotAValue { span });
                };
                let supplied = Parameters::from([(name.clone(), bound)]);
                for later in &mut body.statements[at.saturating_add(1)..] {
                    later.substitute(&supplied)?;
                }
            }
            at = at.saturating_add(1);
        }
        Ok(())
    }
}
