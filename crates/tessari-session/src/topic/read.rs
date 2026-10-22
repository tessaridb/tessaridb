//! `READ FROM`: the messages of a topic after a position.

use std::collections::BTreeMap;

use tessari_ql::{Expr, Span, TableRef};
use tessari_storage::{Catalog, Messages, TableKind, TopicDeclaration, Transaction};
use tessari_types::{DatabaseId, Datetime, NamespaceId, Number, RecordId, TableId, Value};

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

/// How many positions after `after` a read passed over because retention had
/// removed them.
///
/// Everything between the position read after and the first message the topic
/// still holds was removed by retention: positions are dense, and retention is
/// the only thing that removes a message.
pub(super) fn missed(found: &Messages, after: u64) -> u64 {
    let removed = found.first.map_or_else(
        || found.last.unwrap_or(after).saturating_sub(after),
        |first| first.saturating_sub(after.saturating_add(1)),
    );
    removed.saturating_add(found.lapsed)
}

/// What a `READ FROM` statement asked for beyond the topic.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Reading<'a> {
    /// `FOR CONSUMER '<name>'`.
    pub(crate) consumer: Option<&'a str>,
    /// `AFTER <position>`.
    pub(crate) after: Option<&'a Expr>,
    /// `SINCE <instant>` (ADR-0133 D3).
    pub(crate) since: Option<&'a Expr>,
    /// `LIMIT <count>`.
    pub(crate) limit: Option<&'a Expr>,
    /// `PARTITION <n>` (ADR-0131).
    pub(crate) partition: Option<&'a Expr>,
    /// `MEMBER '<name>' [HELD]` (ADR-0133).
    pub(crate) member: Option<&'a tessari_ql::GroupMember>,
}

impl Session<'_> {
    /// Refuse `RETURN POSITION` on a table that is not a topic, before the
    /// write runs (ADR-0129 D10): only a topic gives its records positions.
    pub(crate) async fn must_be_a_topic(
        &self,
        transaction: &mut Transaction<'_>,
        topic: &TableRef,
    ) -> Result<()> {
        let (_, table) = self.resolve_table(transaction, topic).await?;
        let is_topic = Catalog::new(transaction)
            .table(table)
            .await?
            .is_some_and(|definition| matches!(definition.kind, TableKind::Topic(_)));
        if is_topic {
            Ok(())
        } else {
            Err(Error::NotATopic {
                table: topic.name.text.clone(),
                span: topic.span,
            })
        }
    }

    /// The topic a statement naming `table` and `partition` reaches — the
    /// partition of a split topic, or the topic itself (ADR-0131).
    pub(crate) async fn named_partition(
        &self,
        transaction: &mut Transaction<'_>,
        topic: &TableRef,
        table: TableId,
        partition: Option<&Expr>,
        span: Span,
    ) -> Result<TableId> {
        let Some(TableKind::Topic(declared)) = Catalog::new(transaction)
            .table(table)
            .await?
            .map(|definition| definition.kind)
        else {
            return Ok(table);
        };
        self.partition_read(transaction, topic, (table, &declared), partition, span)
            .await
    }

    /// The topics a group on `table` lives in: each partition of a split topic,
    /// or the topic itself (ADR-0131).
    pub(crate) async fn partition_homes(
        &self,
        transaction: &mut Transaction<'_>,
        table: TableId,
    ) -> Result<Vec<TableId>> {
        Ok(match Catalog::new(transaction).table(table).await? {
            Some(tessari_storage::TableDefinition {
                kind:
                    TableKind::Topic(TopicDeclaration {
                        partitions: Some(partitioning),
                        ..
                    }),
                ..
            }) => partitioning.children,
            _ => vec![table],
        })
    }

    /// The topic a read of `topic` reads: the partition it names of a split
    /// topic, or the topic itself — which, never split, is its own partition 0
    /// (ADR-0131).
    async fn partition_read(
        &self,
        transaction: &mut Transaction<'_>,
        topic: &TableRef,
        (table, declared): (TableId, &TopicDeclaration),
        partition: Option<&Expr>,
        span: Span,
    ) -> Result<TableId> {
        let children = declared
            .partitions
            .as_ref()
            .map(|partitioning| partitioning.children.as_slice());
        let Some(partition) = partition else {
            return match children {
                Some(_) => Err(Error::PartitionNotNamed {
                    topic: topic.name.text.clone(),
                    span,
                }),
                None => Ok(table),
            };
        };
        let asked = self
            .whole(
                transaction,
                partition,
                "a partition is a whole number at or above zero",
            )
            .await?;
        let found = match children {
            Some(children) => usize::try_from(asked)
                .ok()
                .and_then(|place| children.get(place).copied()),
            None => (asked == 0).then_some(table),
        };
        found.ok_or_else(|| Error::NoSuchPartition {
            topic: topic.name.text.clone(),
            count: children.map_or(1, |children| {
                u64::try_from(children.len()).unwrap_or(u64::MAX)
            }),
            asked,
            span,
        })
    }

    /// The messages of a topic after a position, oldest first.
    ///
    /// Each answers as `{ position, value }` under its own identity. A named
    /// reader starts after its stored position unless `AFTER` says otherwise,
    /// and the read moves that position to the last message it answers — in
    /// this transaction, so the move commits with whatever else the reader
    /// writes, and is undone with it.
    pub(crate) async fn read_topic(
        &self,
        transaction: &mut Transaction<'_>,
        topic: &TableRef,
        reading: Reading<'_>,
        span: Span,
    ) -> Result<Outcome> {
        let (context, table) = self.resolve_table(transaction, topic).await?;
        let Some(TableKind::Topic(declared)) = Catalog::new(transaction)
            .table(table)
            .await?
            .map(|definition| definition.kind)
        else {
            return Err(Error::NotATopic {
                table: topic.name.text.clone(),
                span,
            });
        };
        let table = self
            .partition_read(
                transaction,
                topic,
                (table, &declared),
                reading.partition,
                span,
            )
            .await?;
        if let Some(consumer) = reading.consumer
            && let Some(state) = Catalog::new(transaction)
                .topic_group(context.namespace, context.database, table, consumer)
                .await?
        {
            // A member may ask for what it holds after a position; nobody may
            // skip the group's work.
            let held = reading.member.is_some_and(|member| member.held);
            if (reading.after.is_some() && !held) || reading.since.is_some() {
                return Err(Error::AfterOnGroup {
                    group: consumer.to_owned(),
                    span,
                });
            }
            let after = match reading.after {
                Some(after) => {
                    self.whole(
                        transaction,
                        after,
                        "a position is a whole number at or above zero",
                    )
                    .await?
                }
                None => 0,
            };
            let limit = match reading.limit {
                Some(limit) => {
                    self.whole(
                        transaction,
                        limit,
                        "a limit is a whole number at or above zero",
                    )
                    .await?
                }
                None => DEFAULT_LIMIT,
            };
            return self
                .read_group(
                    transaction,
                    (&context, table),
                    (topic, consumer),
                    (state, reading.member, after),
                    limit,
                    span,
                )
                .await;
        }
        if reading.member.is_some() {
            return Err(Error::NoGroupForMember {
                consumer: reading.consumer.unwrap_or_default().to_owned(),
                topic: topic.name.text.clone(),
                span,
            });
        }
        let named = match (reading.after, reading.since) {
            (Some(after), _) => Some(
                self.whole(
                    transaction,
                    after,
                    "a position is a whole number at or above zero",
                )
                .await?,
            ),
            (None, Some(since)) => Some(
                self.after_instant(
                    transaction,
                    (context.namespace, context.database, table),
                    since,
                )
                .await?,
            ),
            (None, None) => None,
        };
        let stored = match reading.consumer {
            Some(consumer) => {
                Catalog::new(transaction)
                    .topic_position(context.namespace, context.database, table, consumer)
                    .await?
            }
            None => None,
        };
        let after = named.or(stored).unwrap_or(0);
        let limit = match reading.limit {
            Some(limit) => {
                self.whole(
                    transaction,
                    limit,
                    "a limit is a whole number at or above zero",
                )
                .await?
            }
            None => DEFAULT_LIMIT,
        };
        let found = transaction
            .topic_after(
                context.namespace,
                context.database,
                table,
                after,
                usize::try_from(limit).unwrap_or(usize::MAX),
            )
            .await?;
        let missed = missed(&found, after);
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
                (message.id, with_appended(body, message.appended))
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

    /// The position a read `SINCE` an instant starts after: the one before the
    /// first message appended at or after it, or the last position given when
    /// every message is older (ADR-0133 D3).
    async fn after_instant(
        &self,
        transaction: &mut Transaction<'_>,
        (namespace, database, table): (NamespaceId, DatabaseId, TableId),
        since: &Expr,
    ) -> Result<u64> {
        let reason = "an instant is a datetime at or after 1970";
        let instant = match self.evaluate(transaction, since).await? {
            Value::Datetime(at) => millis_of(at),
            _ => None,
        }
        .ok_or(Error::InvalidPosition {
            reason,
            span: since.span,
        })?;
        Ok(
            match transaction
                .topic_since(namespace, database, table, instant)
                .await?
            {
                Some(first) => first.saturating_sub(1),
                None => transaction
                    .topic_after(namespace, database, table, 0, 0)
                    .await?
                    .last
                    .unwrap_or(0),
            },
        )
    }

    /// A whole number at or above zero, from an expression.
    pub(super) async fn whole(
        &self,
        transaction: &mut Transaction<'_>,
        expr: &Expr,
        reason: &'static str,
    ) -> Result<u64> {
        position_of(self.evaluate(transaction, expr).await?, reason, expr.span)
    }

    /// The positions one expression names: a whole number, or an array of them
    /// — a batch read and bound as one parameter (ADR-0126 D3).
    pub(super) async fn positions(
        &self,
        transaction: &mut Transaction<'_>,
        expr: &Expr,
        reason: &'static str,
    ) -> Result<Vec<u64>> {
        match self.evaluate(transaction, expr).await? {
            Value::Array(items) => items
                .into_iter()
                .map(|item| position_of(item, reason, expr.span))
                .collect(),
            value => Ok(vec![position_of(value, reason, expr.span)?]),
        }
    }
}

/// An instant as whole milliseconds since the epoch, rounded toward it, or
/// `None` for one before it.
fn millis_of(at: Datetime) -> Option<u64> {
    let seconds = u64::try_from(at.seconds()).ok()?;
    seconds
        .checked_mul(1000)?
        .checked_add(u64::from(at.nanos() / 1_000_000))
}

/// The instant `millis` milliseconds after the epoch, as an append instant is
/// kept (ADR-0133 D3).
pub(super) fn instant_of(millis: u64) -> Option<Datetime> {
    Datetime::new(
        i64::try_from(millis / 1000).ok()?,
        u32::try_from((millis % 1000).checked_mul(1_000_000)?).ok()?,
    )
}

/// A message's body with the instant it was appended, when its topic kept
/// one (ADR-0133 D3).
pub(super) fn with_appended(body: Value, appended: Option<u64>) -> Value {
    let at = appended.and_then(instant_of);
    match (body, at) {
        (Value::Object(mut fields), Some(at)) => {
            fields.insert("appended".to_owned(), Value::Datetime(at));
            Value::Object(fields)
        }
        (body, _) => body,
    }
}

/// A whole number at or above zero, or the refusal naming `reason`.
fn position_of(value: Value, reason: &'static str, span: Span) -> Result<u64> {
    match value {
        Value::Number(Number::Integer(held)) => {
            u64::try_from(held).map_err(|_| Error::InvalidPosition { reason, span })
        }
        _ => Err(Error::InvalidPosition { reason, span }),
    }
}
