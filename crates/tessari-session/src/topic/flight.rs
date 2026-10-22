//! What a group holds in flight, member by member (ADR-0086, ADR-0133).
//!
//! A read naming a member records it against every message it hands out, with
//! the instant of the delivery, and keeps the member in the group's registry;
//! `HELD` hands a member back what it already holds. A message retention
//! removed while held is dropped from flight and counted as passed over, as a
//! read of due messages does.

use std::collections::BTreeMap;

use tessari_ql::{Span, TableRef};
use tessari_storage::{
    GroupMemberState, GroupState, InFlight, Message, RecordAddress, Transaction,
};
use tessari_types::{Datetime, RecordId, TableId, Value};

use super::group::{handed, whole};
use super::read::{instant_of, missed};
use crate::context::Context;
use crate::error::Result;
use crate::session::Session;

/// What one hand-out of held messages answered: each message's identity,
/// position and body, and how many were passed over.
pub(super) type Handed = (Vec<(RecordId, u64, Value)>, u64);

impl Session<'_> {
    /// The message at one position, or `None` when retention removed it.
    pub(super) async fn message_at(
        &self,
        transaction: &mut Transaction<'_>,
        context: &Context,
        table: TableId,
        position: u64,
    ) -> Result<Option<Message>> {
        let found = transaction
            .topic_after(
                context.namespace,
                context.database,
                table,
                position.saturating_sub(1),
                1,
            )
            .await?;
        Ok(found
            .messages
            .into_iter()
            .find(|message| message.position == position))
    }

    /// `READ … FOR CONSUMER 'g' MEMBER 'c' HELD [AFTER p]`: the member's own
    /// messages in flight after `after`, in position order, handed to it again
    /// — each delivery counted and its deadline, when the group has one, begun
    /// again.
    pub(super) async fn read_held(
        &self,
        transaction: &mut Transaction<'_>,
        (context, table): (&Context, TableId),
        state: &mut GroupState,
        (member, after, limit): (&str, u64, usize),
        (now, until): (Datetime, Option<Datetime>),
    ) -> Result<Handed> {
        let mine: Vec<u64> = state
            .flight
            .iter()
            .filter(|held| held.position > after && held.member.as_deref() == Some(member))
            .map(|held| held.position)
            .take(limit)
            .collect();
        let mut answered = Vec::with_capacity(mine.len());
        let mut passed = 0_u64;
        for position in mine {
            match self
                .message_at(transaction, context, table, position)
                .await?
            {
                None => {
                    state.flight.retain(|held| held.position != position);
                    passed = passed.saturating_add(1);
                }
                Some(message) => {
                    state.redelivered = state.redelivered.saturating_add(1);
                    let Some(held) = state
                        .flight
                        .iter_mut()
                        .find(|held| held.position == position)
                    else {
                        continue;
                    };
                    held.deliveries = held.deliveries.saturating_add(1);
                    held.until = until;
                    held.delivered = Some(now);
                    answered.push((
                        message.id,
                        position,
                        handed(position, message.value, held.deliveries, message.appended),
                    ));
                }
            }
        }
        Ok((answered, passed))
    }

    /// The due messages, then new ones while the group has room: what a group
    /// read hands out, into `answered`, answering how many were passed over.
    pub(super) async fn hand_out(
        &self,
        transaction: &mut Transaction<'_>,
        (context, table): (&Context, TableId),
        (topic, name): (&TableRef, &str),
        (state, holder): (&mut GroupState, Option<&str>),
        (limit, answered): (usize, &mut Vec<(RecordId, u64, Value)>),
        (now, until, span): (Datetime, Option<Datetime>, Span),
    ) -> Result<u64> {
        let mut missed_count = 0_u64;
        let due: Vec<InFlight> = state
            .flight
            .iter()
            .filter(|held| held.until.is_some_and(|until| until <= now))
            .cloned()
            .collect();
        for held in due {
            if answered.len() >= limit {
                break;
            }
            let message = self
                .message_at(transaction, context, table, held.position)
                .await?;
            let spent = state
                .declaration
                .deliveries
                .is_some_and(|most| held.deliveries >= most);
            if spent || message.is_none() {
                state.flight.retain(|other| other.position != held.position);
            }
            match message {
                None => missed_count = missed_count.saturating_add(1),
                Some(message) if spent => {
                    state.dead_lettered = state.dead_lettered.saturating_add(1);
                    if let Some(letters) = state.declaration.dead_letter {
                        self.dead_letter(
                            transaction,
                            (context, letters),
                            (topic, name),
                            &held,
                            message.value,
                            span,
                        )
                        .await?;
                    }
                }
                Some(message) => {
                    let deliveries = held.deliveries.saturating_add(1);
                    state.redelivered = state.redelivered.saturating_add(1);
                    if let Some(entry) = state
                        .flight
                        .iter_mut()
                        .find(|other| other.position == held.position)
                    {
                        entry.until = until;
                        entry.deliveries = deliveries;
                        entry.member = holder.map(str::to_owned);
                        entry.delivered = Some(now);
                    }
                    answered.push((
                        message.id,
                        held.position,
                        handed(held.position, message.value, deliveries, message.appended),
                    ));
                }
            }
        }

        let room = usize::try_from(state.declaration.width)
            .unwrap_or(usize::MAX)
            .saturating_sub(state.flight.len())
            .min(limit.saturating_sub(answered.len()));
        if room > 0 {
            let after = state.cursor;
            let found = transaction
                .topic_after(context.namespace, context.database, table, after, room)
                .await?;
            let passed = missed(&found, after);
            missed_count = missed_count.saturating_add(passed);
            let reached = found.messages.last().map(|message| message.position);
            for message in found.messages {
                state.flight.push(InFlight {
                    position: message.position,
                    until,
                    deliveries: 1,
                    member: holder.map(str::to_owned),
                    delivered: Some(now),
                    appended: message.appended.and_then(instant_of),
                });
                answered.push((
                    message.id,
                    message.position,
                    handed(message.position, message.value, 1, message.appended),
                ));
            }
            state.cursor = reached.unwrap_or_else(|| after.saturating_add(passed));
        }
        Ok(missed_count)
    }

    /// Append a message given up on to the group's dead letter.
    pub(super) async fn dead_letter(
        &self,
        transaction: &mut Transaction<'_>,
        (context, letters): (&Context, TableId),
        (topic, group): (&TableRef, &str),
        held: &InFlight,
        value: Value,
        span: Span,
    ) -> Result<()> {
        let letter = Value::Object(BTreeMap::from([
            ("topic".to_owned(), Value::from(topic.name.text.as_str())),
            ("group".to_owned(), Value::from(group)),
            ("position".to_owned(), whole(held.position)),
            ("deliveries".to_owned(), whole(held.deliveries)),
            ("value".to_owned(), value),
        ]));
        let id = self
            .free_identity(transaction, context, letters, &letter, span)
            .await?;
        let home = self
            .appended_home(transaction, letters, &letter, span)
            .await?;
        self.put_record(
            transaction,
            RecordAddress::new(context.namespace, context.database, home, id),
            letter,
            span,
        )
        .await
    }
}

/// Note that `member` read under the group at `now`, and that it was handed
/// something when `active`.
pub(super) fn seen(state: &mut GroupState, member: &str, now: Datetime, active: bool) {
    match state.members.iter_mut().find(|known| known.name == member) {
        Some(known) => {
            known.seen = now;
            if active {
                known.active = now;
            }
        }
        None => state.members.push(GroupMemberState {
            name: member.to_owned(),
            seen: now,
            active: now,
        }),
    }
}
