//! `HAND` and `ALTER GROUP … ADD | DROP MEMBER`: who holds a group's messages
//! (ADR-0133, W8d).
//!
//! Handing moves held messages to another member, as a delivery to it: the
//! count goes up, the delivery instant and any deadline begin again. Dropping a
//! member forgets it and what it held — those messages are never handed out
//! again, as an acknowledgement would leave them.

use tessari_ql::{Hand, Handing, Span, TableRef};
use tessari_storage::{Catalog, GroupMemberState, Transaction};
use tessari_types::{Datetime, Value};

use super::flight::seen;
use super::group::{handed, whole};
use crate::AccessPath;
use crate::error::{Error, Result};
use crate::outcome::{Note, Outcome};
use crate::plan::Plan;
use crate::queue::later;
use crate::session::Session;

impl Session<'_> {
    /// `HAND` — the held messages it names, idle at least as long as it asks,
    /// handed to the member; the answer is those messages. One retention
    /// removed is dropped from flight and counted as passed over.
    pub(crate) async fn hand_topic(
        &self,
        transaction: &mut Transaction<'_>,
        hand: &Hand,
        span: Span,
    ) -> Result<Outcome> {
        let (context, table) = self.group_topic(transaction, &hand.topic, span).await?;
        let table = self
            .named_partition(
                transaction,
                &hand.topic,
                table,
                hand.partition.as_ref(),
                span,
            )
            .await?;
        let Some(mut state) = Catalog::new(transaction)
            .topic_group(context.namespace, context.database, table, &hand.consumer)
            .await?
        else {
            return Err(Error::NotAGroup {
                consumer: hand.consumer.clone(),
                topic: hand.topic.name.text.clone(),
                span,
            });
        };
        let reason = "a position is a whole number at or above zero";
        let (named, after, limit) = match &hand.handing {
            Handing::At(positions) => {
                let mut named = Vec::with_capacity(positions.len());
                for position in positions {
                    named.extend(self.positions(transaction, position, reason).await?);
                }
                (Some(named), 0, usize::MAX)
            }
            Handing::After { after, limit } => {
                let after = self.whole(transaction, after, reason).await?;
                let limit = self
                    .whole(
                        transaction,
                        limit,
                        "a limit is a whole number at or above zero",
                    )
                    .await?;
                (None, after, usize::try_from(limit).unwrap_or(usize::MAX))
            }
        };
        let now = crate::call::instant(span)?;
        let idle_since = match &hand.idle {
            None => None,
            Some(idle) => match self.evaluate(transaction, idle).await? {
                Value::Duration(idle) => Some(idle),
                _ => return Err(Error::InvalidIdle { span: idle.span }),
            },
        };
        let until = match state.declaration.deadline {
            Some(deadline) => Some(later(now, deadline, span)?),
            None => None,
        };
        let mut chosen = Vec::new();
        for held in &state.flight {
            let wanted = named.as_ref().map_or(held.position > after, |named| {
                named.contains(&held.position)
            });
            let idle_enough = match (idle_since, held.delivered) {
                (Some(idle), Some(delivered)) => later(delivered, idle, span)? <= now,
                _ => true,
            };
            if wanted && idle_enough && chosen.len() < limit {
                chosen.push(held.position);
            }
        }
        let mut answered = Vec::with_capacity(chosen.len());
        let mut passed = 0_u64;
        for position in chosen {
            match self
                .message_at(transaction, &context, table, position)
                .await?
            {
                None => {
                    state.flight.retain(|held| held.position != position);
                    passed = passed.saturating_add(1);
                }
                Some(message) => {
                    let Some(held) = state
                        .flight
                        .iter_mut()
                        .find(|held| held.position == position)
                    else {
                        continue;
                    };
                    held.deliveries = held.deliveries.saturating_add(1);
                    held.member = Some(hand.member.clone());
                    held.delivered = Some(now);
                    held.until = until;
                    answered.push((
                        message.id,
                        handed(position, message.value, held.deliveries, message.appended),
                    ));
                }
            }
        }
        state.redelivered = state
            .redelivered
            .saturating_add(u64::try_from(answered.len()).unwrap_or(u64::MAX));
        seen(&mut state, &hand.member, now, !answered.is_empty());
        Catalog::new(transaction).put_topic_group(
            context.namespace,
            context.database,
            table,
            &hand.consumer,
            &state,
        );
        let mut notes = Vec::new();
        if passed > 0 {
            notes.push(Note::Lapsed {
                topic: hand.topic.name.text.clone(),
                missed: passed,
            });
        }
        Ok(Outcome::Records {
            records: answered,
            plan: Plan::new(AccessPath::Scan).on(&hand.topic.name.text),
            notes,
            suggestion: None,
            only: false,
        })
    }

    /// `ALTER GROUP … ADD MEMBER` answers 1 when the member is new and 0 when
    /// the group already knew it; `DROP MEMBER` answers how many messages it
    /// held, which the group then forgets.
    pub(crate) async fn change_member(
        &self,
        transaction: &mut Transaction<'_>,
        (name, topic): (&str, &TableRef),
        member: &str,
        add: bool,
        span: Span,
    ) -> Result<Outcome> {
        let (context, table) = self.group_topic(transaction, topic, span).await?;
        let now: Datetime = crate::call::instant(span)?;
        let mut answer = 0_u64;
        for home in self.partition_homes(transaction, table).await? {
            let mut state =
                Self::existing_group(transaction, &context, home, name, topic, span).await?;
            let known = state.members.iter().any(|held| held.name == member);
            if add {
                if !known {
                    state.members.push(GroupMemberState {
                        name: member.to_owned(),
                        seen: now,
                        active: now,
                    });
                    answer = 1;
                }
            } else {
                let before = state.flight.len();
                state
                    .flight
                    .retain(|held| held.member.as_deref() != Some(member));
                let dropped = before.saturating_sub(state.flight.len());
                answer = answer.saturating_add(u64::try_from(dropped).unwrap_or(u64::MAX));
                state.members.retain(|held| held.name != member);
            }
            Catalog::new(transaction).put_topic_group(
                context.namespace,
                context.database,
                home,
                name,
                &state,
            );
        }
        Ok(Outcome::Value(whole(answer)))
    }
}
