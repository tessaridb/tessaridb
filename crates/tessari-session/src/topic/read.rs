//! `READ FROM`: the messages of a topic after a position.

use std::collections::BTreeMap;

use tessari_ql::{Expr, Span, TableRef};
use tessari_storage::{Catalog, TableKind, Transaction};
use tessari_types::{Number, RecordId, Value};

use crate::AccessPath;
use crate::error::{Error, Result};
use crate::outcome::{Note, Outcome};
use crate::plan::Plan;
use crate::session::Session;

/// How many messages a read answers when it names no `LIMIT`.
///
/// A bound rather than everything, because a topic grows without end and a
/// read with no bound would one day answer all of it at once.
const DEFAULT_LIMIT: u64 = 100;

/// What a `READ FROM` statement asked for beyond the topic.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Reading<'a> {
    /// `FOR CONSUMER '<name>'`.
    pub(crate) consumer: Option<&'a str>,
    /// `AFTER <position>`.
    pub(crate) after: Option<&'a Expr>,
    /// `LIMIT <count>`.
    pub(crate) limit: Option<&'a Expr>,
}

impl Session<'_> {
    /// The messages of a topic after a position, oldest first.
    ///
    /// Each answers as `{ position, value }` under its own identity. A named
    /// reader starts after its stored position unless `AFTER` says otherwise,
    /// and the read moves that position to the last message it answers — in
    /// this transaction, so the move commits with whatever else the reader
    /// writes, and is undone with it.
    pub(crate) fn read_topic(
        &self,
        transaction: &mut Transaction<'_>,
        topic: &TableRef,
        reading: Reading<'_>,
        span: Span,
    ) -> Result<Outcome> {
        let (context, table) = self.resolve_table(transaction, topic)?;
        let is_topic = Catalog::new(transaction)
            .table(table)?
            .is_some_and(|definition| matches!(definition.kind, TableKind::Topic(_)));
        if !is_topic {
            return Err(Error::NotATopic {
                table: topic.name.text.clone(),
                span,
            });
        }
        let named = match reading.after {
            Some(after) => Some(self.whole(
                transaction,
                after,
                "a position is a whole number at or above zero",
            )?),
            None => None,
        };
        let stored = match reading.consumer {
            Some(consumer) => Catalog::new(transaction).topic_position(
                context.namespace,
                context.database,
                table,
                consumer,
            )?,
            None => None,
        };
        let after = named.or(stored).unwrap_or(0);
        let limit = match reading.limit {
            Some(limit) => self.whole(
                transaction,
                limit,
                "a limit is a whole number at or above zero",
            )?,
            None => DEFAULT_LIMIT,
        };
        let found = transaction.topic_after(
            context.namespace,
            context.database,
            table,
            after,
            usize::try_from(limit).unwrap_or(usize::MAX),
        )?;
        // Everything between the position read after and the first message the
        // topic still holds was removed by retention: positions are dense, and
        // retention is the only thing that removes a message.
        let removed = found.first.map_or_else(
            || found.last.unwrap_or(after).saturating_sub(after),
            |first| first.saturating_sub(after.saturating_add(1)),
        );
        let missed = removed.saturating_add(found.lapsed);
        let reached = found.messages.last().map(|message| message.position);
        if let Some(consumer) = reading.consumer {
            let moved = reached
                .or(named)
                .or_else(|| (missed > 0).then(|| after.saturating_add(missed)));
            if let Some(position) = moved.filter(|position| Some(*position) != stored) {
                Catalog::new(transaction).set_topic_position(
                    context.namespace,
                    context.database,
                    table,
                    consumer,
                    position,
                );
            }
        }
        let mut notes = Vec::new();
        if missed > 0 {
            notes.push(Note::Lapsed {
                topic: topic.name.text.clone(),
                missed,
            });
        }
        let records: Vec<(RecordId, Value)> = found
            .messages
            .into_iter()
            .map(|message| {
                let body = Value::Object(BTreeMap::from([
                    (
                        "position".to_owned(),
                        Value::Number(Number::Integer(
                            i64::try_from(message.position).unwrap_or(i64::MAX),
                        )),
                    ),
                    ("value".to_owned(), message.value),
                ]));
                (message.id, body)
            })
            .collect();
        Ok(Outcome::Records {
            records,
            plan: Plan::new(AccessPath::Scan).on(&topic.name.text),
            notes,
            suggestion: None,
            only: false,
        })
    }

    /// A whole number at or above zero, from an expression.
    fn whole(
        &self,
        transaction: &mut Transaction<'_>,
        expr: &Expr,
        reason: &'static str,
    ) -> Result<u64> {
        match self.evaluate(transaction, expr)? {
            Value::Number(Number::Integer(held)) => {
                u64::try_from(held).map_err(|_| Error::InvalidPosition {
                    reason,
                    span: expr.span,
                })
            }
            _ => Err(Error::InvalidPosition {
                reason,
                span: expr.span,
            }),
        }
    }
}
