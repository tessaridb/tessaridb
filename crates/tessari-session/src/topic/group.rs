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

use tessari_ql::{Expr, GroupClauses, GroupMember, Span, TableRef};
use tessari_storage::{Catalog, GroupDeclaration, GroupState, TableKind, Transaction};
use tessari_types::{Datetime, Number, RecordId, TableId, Value};

use super::flight::seen;
use super::read::with_appended;
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

pub(super) fn whole(count: u64) -> Value {
    Value::Number(Number::Integer(i64::try_from(count).unwrap_or(i64::MAX)))
}

/// One message as a group read answers it, with the instant it was appended
/// when its topic kept one (ADR-0133).
pub(super) fn handed(position: u64, value: Value, deliveries: u64, appended: Option<u64>) -> Value {
    with_appended(
        Value::Object(BTreeMap::from([
            ("position".to_owned(), whole(position)),
            ("value".to_owned(), value),
            ("deliveries".to_owned(), whole(deliveries)),
        ])),
        appended,
    )
}

impl Session<'_> {
    /// The table a group statement names, refused when it is not a topic.
    pub(crate) async fn group_topic(
        &self,
        transaction: &mut Transaction<'_>,
        topic: &TableRef,
        span: Span,
    ) -> Result<(Context, TableId)> {
        let (context, table) = self.resolve_table(transaction, topic).await?;
        let is_topic = Catalog::new(transaction)
            .table(table)
            .await?
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
    pub(crate) async fn existing_group(
        transaction: &mut Transaction<'_>,
        context: &Context,
        table: TableId,
        name: &str,
        topic: &TableRef,
        span: Span,
    ) -> Result<GroupState> {
        Catalog::new(transaction)
            .topic_group(context.namespace, context.database, table, name)
            .await?
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
    pub(crate) async fn define_group(
        &self,
        transaction: &mut Transaction<'_>,
        name: &str,
        topic: &TableRef,
        if_not_exists: bool,
        clauses: &GroupClauses,
        span: Span,
    ) -> Result<Outcome> {
        let (context, table) = self.group_topic(transaction, topic, span).await?;
        // A split topic's group is one group per partition (ADR-0131).
        let homes = self.partition_homes(transaction, table).await?;
        let first = homes.first().copied().unwrap_or(table);
        if Catalog::new(transaction)
            .topic_group(context.namespace, context.database, first, name)
            .await?
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
                let (_, letters) = self.group_topic(transaction, dead_letter, span).await?;
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
        for home in homes {
            let cursor = Catalog::new(transaction)
                .topic_position(context.namespace, context.database, home, name)
                .await?
                .unwrap_or(0);
            Catalog::new(transaction).put_topic_group(
                context.namespace,
                context.database,
                home,
                name,
                &GroupState::new(declaration, cursor),
            );
        }
        Ok(Outcome::Done)
    }

    /// `DROP GROUP` — the group and what it holds in flight are forgotten.
    pub(crate) async fn drop_group(
        &self,
        transaction: &mut Transaction<'_>,
        name: &str,
        topic: &TableRef,
        span: Span,
    ) -> Result<Outcome> {
        let (context, table) = self.group_topic(transaction, topic, span).await?;
        for home in self.partition_homes(transaction, table).await? {
            Self::existing_group(transaction, &context, home, name, topic, span).await?;
            Catalog::new(transaction).drop_topic_group(
                context.namespace,
                context.database,
                home,
                name,
            );
        }
        Ok(Outcome::Done)
    }

    /// `ALTER GROUP … START AT n` — the group next hands out position `n + 1`,
    /// and what it held in flight is forgotten rather than handed out twice.
    pub(crate) async fn alter_group(
        &self,
        transaction: &mut Transaction<'_>,
        name: &str,
        topic: &TableRef,
        start_at: &Expr,
        span: Span,
    ) -> Result<Outcome> {
        let (context, table) = self.group_topic(transaction, topic, span).await?;
        let cursor = self
            .whole(
                transaction,
                start_at,
                "a position is a whole number at or above zero",
            )
            .await?;
        // Every partition of a split topic starts there (ADR-0131).
        for home in self.partition_homes(transaction, table).await? {
            let mut state =
                Self::existing_group(transaction, &context, home, name, topic, span).await?;
            state.cursor = cursor;
            state.flight.clear();
            Catalog::new(transaction).put_topic_group(
                context.namespace,
                context.database,
                home,
                name,
                &state,
            );
        }
        Ok(Outcome::Done)
    }

    /// `READ FROM … FOR CONSUMER` when the name is a group.
    ///
    /// Messages whose deadline has passed go first, lowest position first, then
    /// new messages after the group's cursor while the group has room in
    /// flight. A due message already delivered as often as the group allows is
    /// dead-lettered instead, and one that retention removed is dropped from
    /// flight and counted as passed over. A read naming a member records it
    /// against what it is handed; with `HELD` it is handed what it holds after
    /// `after` instead (ADR-0133).
    pub(crate) async fn read_group(
        &self,
        transaction: &mut Transaction<'_>,
        (context, table): (&Context, TableId),
        (topic, name): (&TableRef, &str),
        (mut state, member, after): (GroupState, Option<&GroupMember>, u64),
        limit: u64,
        span: Span,
    ) -> Result<Outcome> {
        let now = crate::call::instant(span)?;
        let until = match state.declaration.deadline {
            Some(deadline) => Some(later(now, deadline, span)?),
            None => None,
        };
        let limit = usize::try_from(limit).unwrap_or(usize::MAX);
        let mut answered: Vec<(RecordId, u64, Value)> = Vec::new();
        let holder = member.map(|member| member.name.as_str());

        let missed_count = if let Some(member) = member.filter(|member| member.held) {
            let (held, passed) = self
                .read_held(
                    transaction,
                    (context, table),
                    &mut state,
                    (member.name.as_str(), after, limit),
                    (now, until),
                )
                .await?;
            answered = held;
            passed
        } else {
            self.hand_out(
                transaction,
                (context, table),
                (topic, name),
                (&mut state, holder),
                (limit, &mut answered),
                (now, until, span),
            )
            .await?
        };
        if let Some(holder) = holder {
            seen(&mut state, holder, now, !answered.is_empty());
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
    pub(crate) async fn ack_topic(
        &self,
        transaction: &mut Transaction<'_>,
        topic: (&TableRef, Option<&Expr>),
        consumer: &str,
        positions: &[Expr],
        span: Span,
    ) -> Result<Outcome> {
        let (context, table, mut state, positions) = self
            .settling(transaction, topic, consumer, positions, span)
            .await?;
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
    pub(crate) async fn nack_topic(
        &self,
        transaction: &mut Transaction<'_>,
        topic: (&TableRef, Option<&Expr>),
        consumer: &str,
        (positions, delay): (&[Expr], Option<tessari_types::Duration>),
        span: Span,
    ) -> Result<Outcome> {
        let (context, table, mut state, positions) = self
            .settling(transaction, topic, consumer, positions, span)
            .await?;
        let now = crate::call::instant(span)?;
        let from: Datetime = match delay {
            Some(delay) => later(now, delay, span)?,
            None => now,
        };
        let mut settled = 0_u64;
        for held in &mut state.flight {
            if positions.contains(&held.position) {
                held.until = Some(from);
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
    async fn settling(
        &self,
        transaction: &mut Transaction<'_>,
        (topic, partition): (&TableRef, Option<&Expr>),
        consumer: &str,
        positions: &[Expr],
        span: Span,
    ) -> Result<(Context, TableId, GroupState, Vec<u64>)> {
        let (context, table) = self.group_topic(transaction, topic, span).await?;
        let table = self
            .named_partition(transaction, topic, table, partition, span)
            .await?;
        let Some(state) = Catalog::new(transaction)
            .topic_group(context.namespace, context.database, table, consumer)
            .await?
        else {
            return Err(Error::NotAGroup {
                consumer: consumer.to_owned(),
                topic: topic.name.text.clone(),
                span,
            });
        };
        let mut named = Vec::with_capacity(positions.len());
        for position in positions {
            named.extend(
                self.positions(
                    transaction,
                    position,
                    "a position is a whole number at or above zero",
                )
                .await?,
            );
        }
        Ok((context, table, state, named))
    }
}
