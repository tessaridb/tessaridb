//! Running one statement of a script, inside or outside a transaction.

use super::{Consumer, Session, advised, conflicting, read_version, settle};
use crate::error::{Error, Result};
use crate::outcome::Outcome;
use tessari_ql::{Statement, StatementKind};
use tessari_storage::{Store, Transaction};
use tessari_types::Sequence;

impl<'a> Session<'a> {
    /// One statement, inside the open transaction or in one of its own.
    pub(crate) fn step(
        &mut self,
        store: &'a Store,
        open: &mut Option<(Transaction<'a>, tessari_ql::Span)>,
        statement: &Statement,
    ) -> Result<Outcome> {
        let span = statement.span;
        // **Views are expanded before the statement is authorized, and the
        // order is the security property.** The grant check reads the tables a
        // statement names off the parsed tree, so a view replaced any later
        // would be checked as one table -- its own -- while the read it stands
        // for reached tables nobody granted. Rewriting here means the tree whose
        // tables are counted is the tree that runs.
        let expanded = self.expand_views(store, &statement.kind)?;
        let kind = expanded.as_ref().unwrap_or(&statement.kind);
        self.authorize(store, kind, span)?;
        match kind {
            StatementKind::Use {
                namespace,
                database,
                consumer,
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
                *open = Some((store.begin()?, span));
                self.acknowledge_open = None;
                self.across_open = false;
                Ok(Outcome::Done)
            }
            StatementKind::Commit => {
                let Some((mut transaction, _)) = open.take() else {
                    return Err(Error::NoOpenTransaction { span });
                };
                // The strongest level the transaction's writes or its `COMMIT`
                // asked for (ADR-0106 D2).
                let asked = statement.acknowledge.max(self.acknowledge_open.take());
                let across = statement.across || std::mem::take(&mut self.across_open);
                if across && let Some(parts) = Self::across_parts(&transaction)? {
                    self.drive_across(store, transaction, parts, span)?;
                    return Ok(Outcome::Done);
                }
                let waiting = self.acknowledgement_for(&mut transaction, asked, span)?;
                Self::commit_acknowledged(store, transaction, waiting, span)?;
                Ok(Outcome::Done)
            }
            StatementKind::Cancel => {
                let Some((transaction, _)) = open.take() else {
                    return Err(Error::NoOpenTransaction { span });
                };
                self.acknowledge_open = None;
                self.across_open = false;
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
                transaction.dry_run().map_err(advised)?;
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
                    let mut transaction = store.begin_at(Sequence::new(version.at))?;
                    // A past state is one snapshot, and a gathered part would be
                    // another node's present (G033): withheld, so the read
                    // refuses as it always has.
                    let gather = self.gather.take();
                    let outcome = self.execute(&mut transaction, other, span);
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
                    self.acknowledge_open = self.acknowledge_open.max(statement.acknowledge);
                    self.across_open |= statement.across;
                    let gather = self.gather.take();
                    let outcome = self.execute(transaction, other, span);
                    self.gather = gather;
                    outcome
                }
                (None, None) => {
                    // A lone atomic key-value write is run again on a conflict,
                    // since nothing has been committed and a second run is what
                    // the caller would do (`crate::kv::atomic`).
                    let retried = crate::kv::retried_on_conflict(other);
                    let started = std::time::Instant::now();
                    loop {
                        let mut transaction = store.begin()?;
                        let outcome = self.execute(&mut transaction, other, span)?;
                        if statement.across
                            && let Some(parts) = Self::across_parts(&transaction)?
                        {
                            self.drive_across(store, transaction, parts, span)?;
                            return Ok(outcome);
                        }
                        let waiting = self.acknowledgement_for(
                            &mut transaction,
                            statement.acknowledge,
                            span,
                        )?;
                        match Self::commit_acknowledged(store, transaction, waiting, span) {
                            Ok(()) => {
                                if let StatementKind::DefineRollup { name, source, .. } = other {
                                    self.backfilled(store, source, name)?;
                                }
                                return Ok(outcome);
                            }
                            Err(Error::Store(refusal))
                                if retried
                                    && conflicting(&refusal)
                                    && started.elapsed() < crate::kv::CONFLICT_DEADLINE =>
                            {
                                // Let the winner finish before reading again.
                                std::thread::yield_now();
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
    fn backfilled(
        &mut self,
        store: &'a Store,
        source: &tessari_ql::Name,
        name: &tessari_ql::Name,
    ) -> Result<()> {
        let started = std::time::Instant::now();
        loop {
            let mut transaction = store.begin()?;
            self.backfill_rollup(&mut transaction, source, name)?;
            match settle(transaction) {
                Ok(()) => return Ok(()),
                Err(Error::Store(refusal))
                    if conflicting(&refusal)
                        && started.elapsed() < crate::kv::CONFLICT_DEADLINE =>
                {
                    std::thread::yield_now();
                }
                Err(refusal) => return Err(refusal),
            }
        }
    }
}
