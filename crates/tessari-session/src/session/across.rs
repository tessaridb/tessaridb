//! A participant's half of a transaction across leaders (ADR-0112): the three
//! records a range's leader writes when asked, as the user the asking node
//! verified.
//!
//! # Who decides what may be written
//!
//! This node, from its own catalog. The writes arrive as records rather than
//! as the statements that produced them — the statements ran on the node the
//! transaction ran on, and running them again here would compute other values —
//! so the checks a statement would have passed are asked of the records: the
//! user's tenancy, its authority to write in each database, and its grants on
//! each table. The asking node never widens anybody (ADR-0108 D3).
//!
//! # Majority, always
//!
//! A prepare and a decision answer only once a majority of their range's
//! voters hold them, whatever the namespace's default: a prepare a failover can
//! lose is not a prepare (D3), and an outcome a failover can lose is not an
//! outcome (D4). A resolution waits for its leader alone — it is idempotent,
//! and a lost one is resolved again.

use std::collections::BTreeSet;

use tessari_encoding::{Mutation, RecordValue, TransactionId, TransactionRecord};
use tessari_ql::Span;
use tessari_storage::{Catalog, Kind, RecordAddress, Store, TableKind, Verb};
use tessari_types::{Acknowledge, DatabaseId, NamespaceId, Reach, Sequence, TableId};

mod codec;
mod driver;
mod refusal;

pub use refusal::{AcrossRefusal, PartRefused, RefusalKind};

use super::{Session, advised};
use crate::error::{Error, Result};

/// One record of a transaction across leaders, asked of the leader of the
/// range it belongs to.
#[derive(Debug, Clone, PartialEq)]
pub enum AcrossAsk {
    /// Prepare these writes as intents (D3, D3a).
    Prepare {
        /// The transaction.
        transaction: TransactionId,
        /// The range holding its record.
        coordinator: Reach,
        /// This range's log position the transaction's node had applied.
        seen: Sequence,
        /// The writes that fall in this range, as the transaction buffered them.
        writes: Vec<Mutation>,
    },
    /// Change the transaction's record (D4, D7).
    Decide {
        /// The transaction.
        transaction: TransactionId,
        /// The record as it is to stand.
        record: TransactionRecord,
    },
    /// Answer the record's outcome, aborting it first when it is `PENDING`
    /// past its deadline or absent (D7) — asked of the coordinator range's
    /// leader by a participant holding intents nobody came back to resolve.
    Settle {
        /// The transaction.
        transaction: TransactionId,
        /// The range holding its record.
        coordinator: Reach,
    },
    /// Whether the transaction's intents here are gone for good: none stands,
    /// and a majority holds this range's log through its tail, so no
    /// successor can find one a resolution removed (D12). Asked of a
    /// participant range's leader by the coordinator range's leader.
    Holds {
        /// The transaction.
        transaction: TransactionId,
        /// The participant range asked about.
        range: Reach,
    },
    /// Forget the decided record, every participant having answered that its
    /// intents are gone (D12).
    Forget {
        /// The transaction.
        transaction: TransactionId,
        /// The range holding its record.
        coordinator: Reach,
    },
    /// Resolve the transaction's intents on these records as decided (D4) —
    /// every intent of it this node holds, when no record is named.
    Resolve {
        /// The transaction.
        transaction: TransactionId,
        /// The outcome.
        committed: bool,
        /// The records whose intents to resolve.
        records: Vec<RecordAddress>,
        /// Every participant and where its prepare landed, as the committed
        /// record names them; the resolved versions carry them (D6a).
        participants: Vec<tessari_encoding::Participant>,
    },
}

/// Who carries a record of a transaction across leaders to the node that
/// leads its range, as the user a session verified (ADR-0108, ADR-0112).
///
/// `Send + Sync` because one is shared by every session a node opens, and
/// `Debug` because a session holding one is printed in test failures.
pub trait Participants: core::fmt::Debug + Send + Sync {
    /// Ask node `to` to write `asked`, acting for `user`.
    ///
    /// # Errors
    ///
    /// The refusal in words with its kind — the asking node's, the link's, or
    /// the answering node's — which the coordinator treats as *not prepared*.
    fn ask(
        &self,
        to: [u8; tessari_storage::NODE_ID_LEN],
        user: Option<&tessari_storage::UserDefinition>,
        asked: &AcrossAsk,
    ) -> std::result::Result<AcrossAnswer, PartRefused>;
}

/// What the leader did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AcrossAnswer {
    /// The intents landed at this position and a majority holds them.
    Prepared(Sequence),
    /// The record landed at this position and a majority holds it.
    Decided(Sequence),
    /// The resolution landed at this position, or there was nothing left.
    Resolved(Option<Sequence>),
    /// The record as it now stands — its outcome, and for a committed one
    /// where every prepare landed, which a resolution needs.
    Outcome(TransactionRecord),
    /// Whether an intent of the transaction may still stand here (D12).
    Holding(bool),
    /// The record's forgetting landed at this position and a majority holds it.
    Forgotten(Sequence),
}

impl Session<'_> {
    /// Write one record of a transaction across leaders, as this session's
    /// user, on this node — which must lead the record's range.
    ///
    /// # Errors
    ///
    /// Every refusal a write would meet here — tenancy, authority, grants, the
    /// fence, a conflict — plus `AcrossKind` for a table whose engine has a
    /// write path of its own, and the storage refusals of ADR-0112.
    pub fn answer_across(&mut self, asked: &AcrossAsk) -> Result<AcrossAnswer> {
        let span = Span::new(0, 0);
        let store = self.store;
        match asked {
            AcrossAsk::Prepare {
                transaction,
                coordinator,
                seen,
                writes,
            } => {
                self.may_write_records(store, writes, span)?;
                let mut buffered = store.begin()?;
                for mutation in writes {
                    let address = RecordAddress::new(
                        mutation.namespace,
                        mutation.database,
                        mutation.table,
                        mutation.id.clone(),
                    );
                    match mutation.value.value() {
                        RecordValue::Present(payload) => buffered.put(address, payload.clone()),
                        RecordValue::Tombstone => buffered.delete(address),
                    }
                }
                let waiting = self.acknowledgement_in(
                    &mut buffered,
                    None,
                    Some(Acknowledge::Majority),
                    span,
                )?;
                let committed = buffered
                    .prepare_across(*transaction, *coordinator, *seen)
                    .map_err(advised)?;
                Self::await_acknowledged(store, committed, waiting, span)?;
                Ok(AcrossAnswer::Prepared(committed.sequence))
            }
            AcrossAsk::Decide {
                transaction,
                record,
            } => {
                let home = record
                    .participants
                    .first()
                    .map(|participant| participant.range)
                    .ok_or(Error::Store(tessari_storage::Error::AcrossMalformed {
                        part: "decide",
                        problem: "a record that names no participant",
                    }))?;
                let mut deciding = store.begin()?;
                let waiting = self.acknowledgement_in(
                    &mut deciding,
                    Some(home),
                    Some(Acknowledge::Majority),
                    span,
                )?;
                let committed = deciding
                    .decide_across(*transaction, record.clone())
                    .map_err(advised)?;
                Self::await_acknowledged(store, committed, waiting, span)?;
                Ok(AcrossAnswer::Decided(committed.sequence))
            }
            AcrossAsk::Settle {
                transaction,
                coordinator,
            } => self.settle_across(*transaction, *coordinator),
            AcrossAsk::Holds { transaction, range } => {
                self.holds_across(*transaction, *range, span)
            }
            AcrossAsk::Forget {
                transaction,
                coordinator,
            } => {
                let mut forgetting = store.begin()?;
                let waiting = self.acknowledgement_in(
                    &mut forgetting,
                    Some(*coordinator),
                    Some(Acknowledge::Majority),
                    span,
                )?;
                let committed = forgetting
                    .forget_across(*transaction, *coordinator)
                    .map_err(advised)?;
                Self::await_acknowledged(store, committed, waiting, span)?;
                Ok(AcrossAnswer::Forgotten(committed.sequence))
            }
            AcrossAsk::Resolve {
                transaction,
                committed,
                records,
                participants,
            } => {
                let resolved = store
                    .begin()?
                    .resolve_across(*transaction, *committed, records, participants)
                    .map_err(advised)?;
                Ok(AcrossAnswer::Resolved(
                    resolved.map(|landed| landed.sequence),
                ))
            }
        }
    }

    /// Answer `transaction`'s outcome here, at its coordinator range's
    /// leader, aborting a record that is overdue or was never written.
    ///
    /// A decided record is written again before it is answered. A write is
    /// visible on its leader before its copies exist (ADR-0106 D3), so the
    /// record as read may be held here alone; a participant that dropped its
    /// intents on an abort a failover then lost would leave a transaction the
    /// still-running coordinator can commit, half applied. Writing the same
    /// decision at a majority makes the answer one a failover keeps, and with
    /// it every entry of the log before it.
    fn settle_across(
        &mut self,
        transaction: TransactionId,
        coordinator: Reach,
    ) -> Result<AcrossAnswer> {
        let store = self.store;
        let standing = store.transaction_record(transaction)?;
        let record = match standing {
            Some(record) if record.decision != tessari_encoding::Decision::Pending => record,
            Some(record) if record.deadline > now_millis() => {
                return Ok(AcrossAnswer::Outcome(record));
            }
            Some(record) => TransactionRecord {
                decision: tessari_encoding::Decision::Aborted,
                deadline: 0,
                participants: record.participants,
            },
            None => TransactionRecord {
                decision: tessari_encoding::Decision::Aborted,
                deadline: 0,
                participants: vec![tessari_encoding::Participant {
                    range: coordinator,
                    prepared_at: None,
                }],
            },
        };
        let deciding = AcrossAsk::Decide {
            transaction,
            record: record.clone(),
        };
        match self.answer_across(&deciding) {
            Ok(_) => Ok(AcrossAnswer::Outcome(record)),
            // Decided between the read and the abort — the coordinator's own
            // decision won — so that decision is the one written again. Once:
            // a decided record never changes, so it cannot be refused twice.
            Err(Error::Store(tessari_storage::Error::AcrossDecided { .. })) => {
                self.settle_across(transaction, coordinator)
            }
            Err(refused) => Err(refused),
        }
    }

    /// Whether `transaction` may still hold an intent in `range` here: one
    /// stands, or this range's log through its tail is not yet held by a
    /// majority — a resolution waits for its leader alone, and a successor
    /// missing it would settle the intent against a forgotten record (D12).
    fn holds_across(
        &mut self,
        transaction: TransactionId,
        range: Reach,
        span: Span,
    ) -> Result<AcrossAnswer> {
        let store = self.store;
        if store.holds_intents_of(transaction)? {
            return Ok(AcrossAnswer::Holding(true));
        }
        let mut reading = store.begin()?;
        let waiting =
            self.acknowledgement_in(&mut reading, Some(range), Some(Acknowledge::Majority), span)?;
        reading.rollback();
        let log = store.own_log(range)?;
        let tail = tessari_storage::Committed {
            log,
            sequence: store.committed_tail(log)?,
        };
        Self::await_acknowledged(store, tail, waiting, span)?;
        Ok(AcrossAnswer::Holding(false))
    }

    /// Refuse writes this session's user could not have made here, or into a
    /// table whose engine has a write path of its own.
    fn may_write_records(&mut self, store: &Store, writes: &[Mutation], span: Span) -> Result<()> {
        let open = self.refresh(store)?;
        let tables: BTreeSet<(NamespaceId, DatabaseId, TableId)> = writes
            .iter()
            .map(|mutation| (mutation.namespace, mutation.database, mutation.table))
            .collect();
        let mut reading = store.begin()?;
        let catalog = Catalog::new(&mut reading);
        let user = self.identity.user();
        let grants = match user {
            Some(user) => catalog.grants_for(user.id)?,
            None => Vec::new(),
        };
        for (namespace, database, table) in tables {
            let Some(definition) = catalog.table(table)? else {
                return Err(Error::Store(tessari_storage::Error::AcrossMalformed {
                    part: "prepare",
                    problem: "a write into a table this node does not know",
                }));
            };
            // A vault seals with this process's key, and a bucket, a space, a
            // topic, a queue or a series keeps something beside its records that
            // a prepared write would bypass.
            let plain = matches!(
                definition.kind,
                TableKind::Table | TableKind::Collection | TableKind::Edge(_)
            ) && !definition.is_vault();
            if !plain {
                return Err(Error::AcrossKind {
                    table: definition.name,
                    span,
                });
            }
            // A store with no user yet is open to everyone, as every statement
            // on it is; once one exists, every write is somebody's.
            if open {
                continue;
            }
            let Some(user) = user else {
                return Err(Error::NotSignedIn { span });
            };
            self.permits(namespace, database, &definition.name, span)?;
            if !user
                .authorities
                .permits(Kind::Write, Reach::Database(namespace, database))
            {
                return Err(Error::RoleForbids {
                    role: user.role.map_or("authorities", tessari_storage::Role::name),
                    needs: Kind::Write.name(),
                    span,
                });
            }
            let granted = grants.is_empty()
                || grants
                    .iter()
                    .any(|grant| grant.table == table && grant.verbs.contains(&Verb::Write));
            if !granted {
                return Err(Error::NotGranted {
                    user: user.name.clone(),
                    table: definition.name,
                    needs: Verb::Write.name(),
                    span,
                });
            }
        }
        reading.rollback();
        Ok(())
    }
}

/// Milliseconds since the Unix epoch; zero for a clock before it.
pub(super) fn now_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |since| {
            u64::try_from(since.as_millis()).unwrap_or(u64::MAX)
        })
}
