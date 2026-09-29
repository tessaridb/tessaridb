//! Consumer groups (G042, ADR-0086): readers under one name who share a topic's
//! messages and acknowledge each one.
//!
//! A read by a member hands out messages and records each in flight with a
//! deadline; `ACK` forgets a message, `NACK` makes it deliverable again, and a
//! deadline that passes makes it deliverable again on its own. A message handed
//! out as often as the group allows is dead-lettered instead of handed out
//! again. Nothing sweeps: every change happens in the statement that observes
//! it, against a deadline written in the group's record.

use std::collections::BTreeMap;

use tessari_ql::{Expr, GroupClauses, Span, TableRef};
use tessari_storage::{
    Catalog, GroupDeclaration, GroupState, InFlight, RecordAddress, TableKind, Transaction,
};
use tessari_types::{Datetime, Number, RecordId, TableId, Value};

use super::read::missed;
use crate::AccessPath;
use crate::context::Context;
use crate::error::{Error, Result};
use crate::outcome::{Note, Outcome};
use crate::plan::Plan;
use crate::queue::later;
use crate::session::Session;

/// A group's width when its declaration names none: one message at a time,
/// which is the only width that keeps the topic's order.
const DEFAULT_WIDTH: u64 = 1;

fn whole(count: u64) -> Value {
    Value::Number(Number::Integer(i64::try_from(count).unwrap_or(i64::MAX)))
}

/// One message as a group read answers it.
fn handed(position: u64, value: Value, deliveries: u64) -> Value {
    Value::Object(BTreeMap::from([
        ("position".to_owned(), whole(position)),
        ("value".to_owned(), value),
        ("deliveries".to_owned(), whole(deliveries)),
    ]))
}

impl Session<'_> {
    /// The table a group statement names, refused when it is not a topic.
    pub(crate) fn group_topic(
        &self,
        transaction: &mut Transaction<'_>,
        topic: &TableRef,
        span: Span,
    ) -> Result<(Context, TableId)> {
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
        Ok((context, table))
    }

    /// The group `name` on this topic, refused when there is none.
    pub(crate) fn existing_group(
        transaction: &mut Transaction<'_>,
        context: &Context,
        table: TableId,
        name: &str,
        topic: &TableRef,
        span: Span,
    ) -> Result<GroupState> {
        Catalog::new(transaction)
            .topic_group(context.namespace, context.database, table, name)?
            .ok_or_else(|| Error::NoSuchGroup {
                group: name.to_owned(),
                topic: topic.name.text.clone(),
                span,
            })
    }

    /// `DEFINE GROUP`.
    ///
    /// A group takes over a reader's stored position when one exists under its
    /// name, so declaring a group for readers that already read the topic does
    /// not hand them everything again; otherwise it starts at the beginning, as
    /// a named reader does.
    pub(crate) fn define_group(
        &self,
        transaction: &mut Transaction<'_>,
        name: &str,
        topic: &TableRef,
        if_not_exists: bool,
        clauses: &GroupClauses,
        span: Span,
    ) -> Result<Outcome> {
        let (context, table) = self.group_topic(transaction, topic, span)?;
        if Catalog::new(transaction)
            .topic_group(context.namespace, context.database, table, name)?
            .is_some()
        {
            if if_not_exists {
                return Ok(Outcome::Done);
            }
            return Err(Error::GroupExists {
                group: name.to_owned(),
                topic: topic.name.text.clone(),
                span,
            });
        }
        let dead_letter = match &clauses.dead_letter {
            Some(dead_letter) => {
                let (_, letters) = self.group_topic(transaction, dead_letter, span)?;
                if letters == table {
                    return Err(Error::DeadLetterIsTheTopic {
                        topic: topic.name.text.clone(),
                        span,
                    });
                }
                Some(letters)
            }
            None => None,
        };
        let declaration = GroupDeclaration {
            deadline: clauses.deadline,
            deliveries: clauses.deliveries,
            width: clauses.in_flight.unwrap_or(DEFAULT_WIDTH),
            dead_letter,
        };
        let cursor = Catalog::new(transaction)
            .topic_position(context.namespace, context.database, table, name)?
            .unwrap_or(0);
        Catalog::new(transaction).put_topic_group(
            context.namespace,
            context.database,
            table,
            name,
            &GroupState::new(declaration, cursor),
        );
        Ok(Outcome::Done)
    }

    /// `DROP GROUP` — the group and what it holds in flight are forgotten.
    pub(crate) fn drop_group(
        &self,
        transaction: &mut Transaction<'_>,
        name: &str,
        topic: &TableRef,
        span: Span,
    ) -> Result<Outcome> {
        let (context, table) = self.group_topic(transaction, topic, span)?;
        Self::existing_group(transaction, &context, table, name, topic, span)?;
        Catalog::new(transaction).drop_topic_group(
            context.namespace,
            context.database,
            table,
            name,
        );
        Ok(Outcome::Done)
    }

    /// `ALTER GROUP … START AT n` — the group next hands out position `n + 1`,
    /// and what it held in flight is forgotten rather than handed out twice.
    pub(crate) fn alter_group(
        &self,
        transaction: &mut Transaction<'_>,
        name: &str,
        topic: &TableRef,
        start_at: &Expr,
        span: Span,
    ) -> Result<Outcome> {
        let (context, table) = self.group_topic(transaction, topic, span)?;
        let mut state = Self::existing_group(transaction, &context, table, name, topic, span)?;
        state.cursor = self.whole(
            transaction,
            start_at,
            "a position is a whole number at or above zero",
        )?;
        state.flight.clear();
        Catalog::new(transaction).put_topic_group(
            context.namespace,
            context.database,
            table,
            name,
            &state,
        );
        Ok(Outcome::Done)
    }

    /// `READ FROM … FOR CONSUMER` when the name is a group.
    ///
    /// Messages whose deadline has passed go first, lowest position first, then
    /// new messages after the group's cursor while the group has room in
    /// flight. A due message already delivered as often as the group allows is
    /// dead-lettered instead, and one that retention removed is dropped from
    /// flight and counted as passed over.
    pub(crate) fn read_group(
        &self,
        transaction: &mut Transaction<'_>,
        (context, table): (&Context, TableId),
        (topic, name): (&TableRef, &str),
        mut state: GroupState,
        limit: u64,
        span: Span,
    ) -> Result<Outcome> {
        let now = crate::call::instant(span)?;
        let until = later(now, state.declaration.deadline, span)?;
        let limit = usize::try_from(limit).unwrap_or(usize::MAX);
        let mut answered: Vec<(RecordId, u64, Value)> = Vec::new();
        let mut missed_count = 0_u64;

        let due: Vec<InFlight> = state
            .flight
            .iter()
            .copied()
            .filter(|held| held.until <= now)
            .collect();
        for held in due {
            if answered.len() >= limit {
                break;
            }
            let message = self.message_at(transaction, context, table, held.position)?;
            let spent = state
                .declaration
                .deliveries
                .is_some_and(|most| held.deliveries >= most);
            if spent || message.is_none() {
                state.flight.retain(|other| other.position != held.position);
            }
            match message {
                None => missed_count = missed_count.saturating_add(1),
                Some((_, value)) if spent => {
                    state.dead_lettered = state.dead_lettered.saturating_add(1);
                    if let Some(letters) = state.declaration.dead_letter {
                        self.dead_letter(
                            transaction,
                            (context, letters),
                            (topic, name),
                            held,
                            value,
                            span,
                        )?;
                    }
                }
                Some((id, value)) => {
                    let deliveries = held.deliveries.saturating_add(1);
                    state.redelivered = state.redelivered.saturating_add(1);
                    if let Some(entry) = state
                        .flight
                        .iter_mut()
                        .find(|other| other.position == held.position)
                    {
                        entry.until = until;
                        entry.deliveries = deliveries;
                    }
                    answered.push((id, held.position, handed(held.position, value, deliveries)));
                }
            }
        }

        let room = usize::try_from(state.declaration.width)
            .unwrap_or(usize::MAX)
            .saturating_sub(state.flight.len())
            .min(limit.saturating_sub(answered.len()));
        if room > 0 {
            let after = state.cursor;
            let found =
                transaction.topic_after(context.namespace, context.database, table, after, room)?;
            let passed = missed(&found, after);
            missed_count = missed_count.saturating_add(passed);
            let reached = found.messages.last().map(|message| message.position);
            for message in found.messages {
                state.flight.push(InFlight {
                    position: message.position,
                    until,
                    deliveries: 1,
                });
                answered.push((
                    message.id,
                    message.position,
                    handed(message.position, message.value, 1),
                ));
            }
            state.cursor = reached.unwrap_or_else(|| after.saturating_add(passed));
        }
        state.flight.sort_by_key(|held| held.position);
        answered.sort_by_key(|(_, position, _)| *position);

        Catalog::new(transaction).put_topic_group(
            context.namespace,
            context.database,
            table,
            name,
            &state,
        );
        let mut notes = Vec::new();
        if missed_count > 0 {
            notes.push(Note::Lapsed {
                topic: topic.name.text.clone(),
                missed: missed_count,
            });
        }
        Ok(Outcome::Records {
            records: answered
                .into_iter()
                .map(|(id, _, body)| (id, body))
                .collect(),
            plan: Plan::new(AccessPath::Scan).on(&topic.name.text),
            notes,
            suggestion: None,
            only: false,
        })
    }

    /// `ACK` — answers how many of the positions were in flight and are now
    /// done. A position not in flight counts nothing and refuses nothing, so an
    /// acknowledgement retried after a lost answer is harmless.
    pub(crate) fn ack_topic(
        &self,
        transaction: &mut Transaction<'_>,
        topic: &TableRef,
        consumer: &str,
        positions: &[Expr],
        span: Span,
    ) -> Result<Outcome> {
        let (context, table, mut state, positions) =
            self.settling(transaction, topic, consumer, positions, span)?;
        let before = state.flight.len();
        state
            .flight
            .retain(|held| !positions.contains(&held.position));
        let settled = before.saturating_sub(state.flight.len());
        if settled > 0 {
            Catalog::new(transaction).put_topic_group(
                context.namespace,
                context.database,
                table,
                consumer,
                &state,
            );
        }
        Ok(Outcome::Value(whole(
            u64::try_from(settled).unwrap_or(u64::MAX),
        )))
    }

    /// `NACK` — the positions become deliverable now, or after the delay, and
    /// the answer is how many were in flight.
    pub(crate) fn nack_topic(
        &self,
        transaction: &mut Transaction<'_>,
        topic: &TableRef,
        consumer: &str,
        (positions, delay): (&[Expr], Option<tessari_types::Duration>),
        span: Span,
    ) -> Result<Outcome> {
        let (context, table, mut state, positions) =
            self.settling(transaction, topic, consumer, positions, span)?;
        let now = crate::call::instant(span)?;
        let from: Datetime = match delay {
            Some(delay) => later(now, delay, span)?,
            None => now,
        };
        let mut settled = 0_u64;
        for held in &mut state.flight {
            if positions.contains(&held.position) {
                held.until = from;
                settled = settled.saturating_add(1);
            }
        }
        if settled > 0 {
            Catalog::new(transaction).put_topic_group(
                context.namespace,
                context.database,
                table,
                consumer,
                &state,
            );
        }
        Ok(Outcome::Value(whole(settled)))
    }

    /// The topic, the group and the positions an `ACK` or `NACK` names.
    fn settling(
        &self,
        transaction: &mut Transaction<'_>,
        topic: &TableRef,
        consumer: &str,
        positions: &[Expr],
        span: Span,
    ) -> Result<(Context, TableId, GroupState, Vec<u64>)> {
        let (context, table) = self.group_topic(transaction, topic, span)?;
        let Some(state) = Catalog::new(transaction).topic_group(
            context.namespace,
            context.database,
            table,
            consumer,
        )?
        else {
            return Err(Error::NotAGroup {
                consumer: consumer.to_owned(),
                topic: topic.name.text.clone(),
                span,
            });
        };
        let mut named = Vec::with_capacity(positions.len());
        for position in positions {
            named.push(self.whole(
                transaction,
                position,
                "a position is a whole number at or above zero",
            )?);
        }
        Ok((context, table, state, named))
    }

    /// The message at one position, or `None` when retention removed it.
    fn message_at(
        &self,
        transaction: &mut Transaction<'_>,
        context: &Context,
        table: TableId,
        position: u64,
    ) -> Result<Option<(RecordId, Value)>> {
        let found = transaction.topic_after(
            context.namespace,
            context.database,
            table,
            position.saturating_sub(1),
            1,
        )?;
        Ok(found
            .messages
            .into_iter()
            .find(|message| message.position == position)
            .map(|message| (message.id, message.value)))
    }

    /// Append a message given up on to the group's dead letter.
    fn dead_letter(
        &self,
        transaction: &mut Transaction<'_>,
        (context, letters): (&Context, TableId),
        (topic, group): (&TableRef, &str),
        held: InFlight,
        value: Value,
        span: Span,
    ) -> Result<()> {
        let id = self.free_identity(transaction, context, letters, span)?;
        let letter = Value::Object(BTreeMap::from([
            ("topic".to_owned(), Value::from(topic.name.text.as_str())),
            ("group".to_owned(), Value::from(group)),
            ("position".to_owned(), whole(held.position)),
            ("deliveries".to_owned(), whole(held.deliveries)),
            ("value".to_owned(), value),
        ]));
        self.put_record(
            transaction,
            RecordAddress::new(context.namespace, context.database, letters, id),
            letter,
            span,
        )
    }
}
