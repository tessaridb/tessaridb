//! Running one statement of a script, inside or outside a transaction.

use super::{Consumer, Session, advised, conflicting, read_version, settle};
use crate::error::{Error, Result};
use crate::outcome::Outcome;
use tessari_ql::{Answer, CreateTarget, Statement, StatementKind, TableRef};
use tessari_storage::{
    Catalog, Store, TableDefinition, TableKind, TopicDeclaration, Transaction, back_off,
};
use tessari_types::{Number, Sequence, Value};

impl<'a> Session<'a> {
    /// One statement, inside the open transaction or in one of its own.
    pub(crate) async fn step(
        &mut self,
        store: &'a Store,
        open: &mut Option<(Transaction<'a>, tessari_ql::Span)>,
        statement: &Statement,
    ) -> Result<Outcome> {
        let span = statement.span;
        self.now = std::time::SystemTime::now();
        // **Views are expanded before the statement is authorized, and the
        // order is the security property.** The grant check reads the tables a
        // statement names off the parsed tree, so a view replaced any later
        // would be checked as one table -- its own -- while the read it stands
        // for reached tables nobody granted. Rewriting here means the tree whose
        // tables are counted is the tree that runs.
        let expanded = self.expand_views(store, &statement.kind).await?;
        let kind = expanded.as_ref().unwrap_or(&statement.kind);
        self.authorize(store, kind, span).await?;
        // A write is sent to the leader by its commit; a read of a held space
        // is sent there here, before it can answer from this node (W5d).
        if matches!(crate::Effect::of(kind), crate::Effect::Read) {
            self.held_read_here(store, kind, span).await?;
        }
        match kind {
            StatementKind::Use {
                namespace,
                database,
                consumer,
                consumer_parameter: _,
            } => {
                // Recorded, not resolved: the namespace this names may be
                // defined by a later statement of the same transaction.
                if let Some(name) = namespace {
                    self.namespace = Some(name.text.clone());
                }
                if let Some(name) = database {
                    self.database = Some(name.text.clone());
                }
                if let Some(name) = consumer {
                    // A fresh instance on every declaration, including a
                    // re-declaration of the same name. A session that says who
                    // it is again is a new claimant from the queue's side, and
                    // reusing the value would let `RELEASE ALL` reach holds the
                    // previous declaration took — which is the reuse §1 of the
                    // design forbids, arriving from inside one session.
                    self.consumer = Some(Consumer {
                        name: name.clone(),
                        instance: crate::ticket::instance(),
                    });
                }
                Ok(Outcome::Done)
            }
            StatementKind::Begin => {
                if open.is_some() {
                    return Err(Error::NestedTransaction { span });
                }
                *open = Some((store.begin().await?, span));
                self.acknowledge_open = None;
                self.across_open = false;
                self.savepoints.clear();
                Ok(Outcome::Done)
            }
            StatementKind::Savepoint { name } => {
                let Some((transaction, _)) = open.as_ref() else {
                    return Err(Error::SavepointOutsideATransaction { span });
                };
                self.savepoints
                    .push((name.text.clone(), transaction.savepoint()));
                Ok(Outcome::Done)
            }
            StatementKind::RollbackTo { name } => {
                let Some((transaction, _)) = open.as_mut() else {
                    return Err(Error::SavepointOutsideATransaction { span });
                };
                let at = self.savepoint_named(name)?;
                if let Some((_, point)) = self.savepoints.get(at) {
                    transaction.roll_back_to(point).map_err(|why| {
                        Error::SavepointCrossesClaim {
                            name: name.text.clone(),
                            what: match why {
                                tessari_storage::Unreturnable::Claim => {
                                    "took a queue claim, which the claim register holds until \
                                     the transaction ends"
                                }
                                tessari_storage::Unreturnable::Across => {
                                    "handed work to other leaders"
                                }
                            },
                            span,
                        }
                    })?;
                }
                // The savepoint stays, so it can be rolled back to again.
                self.savepoints.truncate(at.saturating_add(1));
                Ok(Outcome::Done)
            }
            StatementKind::ReleaseSavepoint { name } => {
                if open.is_none() {
                    return Err(Error::SavepointOutsideATransaction { span });
                }
                let at = self.savepoint_named(name)?;
                self.savepoints.truncate(at);
                Ok(Outcome::Done)
            }
            StatementKind::Commit => {
                let Some((mut transaction, _)) = open.take() else {
                    return Err(Error::NoOpenTransaction { span });
                };
                self.savepoints.clear();
                // The strongest level the transaction's writes or its `COMMIT`
                // asked for (ADR-0106 D2).
                let asked = statement.acknowledge.max(self.acknowledge_open.take());
                let across = statement.across || std::mem::take(&mut self.across_open);
                if across && let Some(parts) = Self::across_parts(&transaction).await? {
                    self.drive_across(store, transaction, parts, span).await?;
                    return Ok(Outcome::Done);
                }
                let waiting = self
                    .acknowledgement_for(&mut transaction, asked, span)
                    .await?;
                Self::commit_acknowledged(store, transaction, waiting, span).await?;
                Ok(Outcome::Done)
            }
            StatementKind::Cancel => {
                let Some((transaction, _)) = open.take() else {
                    return Err(Error::NoOpenTransaction { span });
                };
                self.acknowledge_open = None;
                self.across_open = false;
                self.savepoints.clear();
                transaction.rollback();
                Ok(Outcome::Done)
            }
            // Closes the transaction exactly as its two siblings do. A rehearsal
            // that left the transaction open would invite a second one against a
            // snapshot the first had already answered for.
            StatementKind::Verify => {
                let Some((transaction, _)) = open.take() else {
                    return Err(Error::NoOpenTransaction { span });
                };
                self.acknowledge_open = None;
                self.across_open = false;
                self.savepoints.clear();
                transaction.dry_run().await.map_err(advised)?;
                Ok(Outcome::Done)
            }
            // Its backfill is a second commit after the declaration's, which
            // an enclosing transaction would hold back (ADR-0088 §6).
            StatementKind::DefineRollup { .. } if open.is_some() => {
                Err(Error::RollupInTransaction { span })
            }
            // It runs its script in a transaction of its own, which an enclosing
            // one would hold back or silently split from.
            StatementKind::Restore { .. } if open.is_some() => Err(Error::RestoreRefused {
                reason: "it runs in a transaction of its own, outside `BEGIN … COMMIT`".to_owned(),
            }),
            other => match (read_version(other), open.as_mut()) {
                // A transaction is one point in the store's history — that is
                // what a snapshot is — so a statement inside one cannot ask for
                // a different one. Refused rather than silently answered at the
                // transaction's own snapshot, which would make the clause read
                // as though it had been honoured.
                (Some(version), Some(_)) => {
                    Err(Error::VersionInsideTransaction { span: version.span })
                }
                (Some(version), None) => {
                    let mut transaction = store.begin_at(Sequence::new(version.at)).await?;
                    // A past state is one snapshot, and a gathered part would be
                    // another node's present (G033): withheld, so the read
                    // refuses as it always has.
                    let gather = self.gather.take();
                    let outcome = self.execute(&mut transaction, other, span).await;
                    self.gather = gather;
                    let outcome = outcome?;
                    // Rolled back, not committed. A read of the past has nothing
                    // to commit, and a transaction holding an old snapshot is
                    // exactly what a commit would have to reconcile against the
                    // present.
                    transaction.rollback();
                    Ok(outcome)
                }
                // A transaction is one snapshot too, for the same reason.
                (None, Some((transaction, _))) => {
                    if answers_position(other).is_some() {
                        return Err(Error::PositionNotDecided { span });
                    }
                    self.acknowledge_open = self.acknowledge_open.max(statement.acknowledge);
                    self.across_open |= statement.across;
                    let gather = self.gather.take();
                    let outcome = self.execute(transaction, other, span).await;
                    self.gather = gather;
                    match outcome {
                        Ok(outcome) => Ok(outcome),
                        Err(refusal) => Err(fell_behind(refusal, transaction).await?),
                    }
                }
                (None, None) => {
                    // A lone atomic key-value write is run again on a conflict,
                    // since nothing has been committed and a second run is what
                    // the caller would do (`crate::kv::atomic`).
                    let retried = crate::kv::retried_on_conflict(other);
                    let started = std::time::Instant::now();
                    let mut attempt = 0_u32;
                    loop {
                        let mut transaction = store.begin().await?;
                        let outcome = match self.execute(&mut transaction, other, span).await {
                            Ok(outcome) => outcome,
                            Err(refusal) => return Err(fell_behind(refusal, &transaction).await?),
                        };
                        if statement.across
                            && let Some(parts) = Self::across_parts(&transaction).await?
                        {
                            self.drive_across(store, transaction, parts, span).await?;
                            return Ok(outcome);
                        }
                        let waiting = self
                            .acknowledgement_for(&mut transaction, statement.acknowledge, span)
                            .await?;
                        match Self::commit_acknowledged(store, transaction, waiting, span).await {
                            Ok(()) => {
                                if let StatementKind::DefineRollup { name, source, .. } = other {
                                    self.backfilled(store, source, name).await?;
                                }
                                if let Some(table) = answers_position(other) {
                                    return self.positioned(store, table, outcome).await;
                                }
                                return Ok(outcome);
                            }
                            Err(Error::Store(refusal))
                                if retried
                                    && conflicting(&refusal)
                                    && started.elapsed() < crate::kv::CONFLICT_DEADLINE =>
                            {
                                // Let the winner finish before reading again,
                                // at a jittered distance so that the losers do
                                // not all read again together.
                                attempt = attempt.saturating_add(1);
                                back_off(attempt).await;
                            }
                            Err(refusal) => return Err(refusal),
                        }
                    }
                }
            },
        }
    }

    /// `DEFINE ROLLUP`'s second commit, retried while a concurrent raw write
    /// lands on a row it also writes (ADR-0088 §6).
    async fn backfilled(
        &mut self,
        store: &'a Store,
        source: &tessari_ql::Name,
        name: &tessari_ql::Name,
    ) -> Result<()> {
        let started = std::time::Instant::now();
        let mut attempt = 0_u32;
        loop {
            let mut transaction = store.begin().await?;
            self.backfill_rollup(&mut transaction, source, name).await?;
            match settle(transaction).await {
                Ok(()) => return Ok(()),
                Err(Error::Store(refusal))
                    if conflicting(&refusal)
                        && started.elapsed() < crate::kv::CONFLICT_DEADLINE =>
                {
                    attempt = attempt.saturating_add(1);
                    back_off(attempt).await;
                }
                Err(refusal) => return Err(refusal),
            }
        }
    }
}

impl<'a> Session<'a> {
    /// The positions a committed append's messages took, in place of their
    /// identities (ADR-0129 D10).
    ///
    /// Read after the commit, because that is where a position is decided; a
    /// message another node is still applying answers `NONE`.
    async fn positioned(
        &self,
        store: &'a Store,
        table: &TableRef,
        outcome: Outcome,
    ) -> Result<Outcome> {
        let Outcome::Keys(ids) = outcome else {
            return Ok(outcome);
        };
        let mut transaction = store.begin().await?;
        let (context, id) = self.resolve_table(&mut transaction, table).await?;
        let children = match Catalog::new(&mut transaction).table(id).await? {
            Some(TableDefinition {
                kind:
                    TableKind::Topic(TopicDeclaration {
                        partitions: Some(partitioning),
                        ..
                    }),
                ..
            }) => partitioning.children,
            _ => Vec::new(),
        };
        if !children.is_empty() {
            // A split topic's message holds a position in the partition it
            // landed in, so the answer names both (ADR-0131).
            let mut placed: Vec<Option<(usize, u64)>> = vec![None; ids.len()];
            for (partition, child) in children.into_iter().enumerate() {
                let positions = transaction
                    .topic_positions(context.namespace, context.database, child, &ids)
                    .await?;
                for (slot, position) in placed.iter_mut().zip(positions) {
                    if slot.is_none() {
                        *slot = position.map(|position| (partition, position));
                    }
                }
            }
            let whole = |number: u64| {
                Value::Number(Number::Integer(i64::try_from(number).unwrap_or(i64::MAX)))
            };
            return Ok(Outcome::Value(Value::Array(
                placed
                    .into_iter()
                    .map(|place| {
                        place.map_or(Value::None, |(partition, position)| {
                            Value::Object(std::collections::BTreeMap::from([
                                (
                                    "partition".to_owned(),
                                    whole(u64::try_from(partition).unwrap_or(u64::MAX)),
                                ),
                                ("position".to_owned(), whole(position)),
                            ]))
                        })
                    })
                    .collect(),
            )));
        }
        let positions = transaction
            .topic_positions(context.namespace, context.database, id, &ids)
            .await?;
        Ok(Outcome::Value(Value::Array(
            positions
                .into_iter()
                .map(|position| {
                    position.map_or(Value::None, |held| {
                        Value::Number(Number::Integer(i64::try_from(held).unwrap_or(i64::MAX)))
                    })
                })
                .collect(),
        )))
    }
}

/// The topic a `RETURN POSITION` append writes, if the statement is one.
/// A refusal a read met only because another commit landed after its
/// snapshot, named for what answers it: running the read again (DB-V15).
///
/// A walk refused for it is already named so; a `USING` assertion an index
/// failed because the transaction had fallen behind the tail is renamed here,
/// so the caller sees the `retry` class rather than a fault in the statement.
/// The store does not run it again itself: the retry is the caller's.
async fn fell_behind(refusal: Error, transaction: &Transaction<'_>) -> Result<Error> {
    Ok(match refusal {
        Error::IndexNotUsed { span, .. } | Error::PathNotTaken { span, .. }
            if !transaction.reads_the_past() && !transaction.indexes_are_current().await? =>
        {
            Error::ReadFellBehind {
                table: String::new(),
                span,
            }
        }
        other => other,
    })
}

fn answers_position(kind: &StatementKind) -> Option<&TableRef> {
    match kind {
        StatementKind::Insert {
            table,
            answer: Answer::Position,
            ..
        } => Some(table),
        StatementKind::Create {
            target: CreateTarget::Generated(table),
            answer: Answer::Position,
            ..
        } => Some(table),
        StatementKind::Create {
            target: CreateTarget::Named(target),
            answer: Answer::Position,
            ..
        } => Some(&target.table),
        _ => None,
    }
}

impl Session<'_> {
    /// Where the newest savepoint of this name stands among those held.
    fn savepoint_named(&self, name: &tessari_ql::Name) -> Result<usize> {
        self.savepoints
            .iter()
            .rposition(|(held, _)| held == &name.text)
            .ok_or_else(|| Error::NoSuchSavepoint {
                name: name.text.clone(),
                span: name.span,
            })
    }
}
