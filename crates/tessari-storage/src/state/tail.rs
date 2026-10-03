//! What a state carries of transactions across leaders, after its records
//! (ADR-0112 D9a).
//!
//! # Why the cut needs no closure
//!
//! A reader decides whether it sees a transaction across leaders from its own
//! snapshot (D6a): committed as far as the node knows, and every part this node
//! holds landed at or below the snapshot. A state read at one version is that
//! snapshot, so it is whole or absent for every transaction already — if the
//! restore can decide as the source did. That needs what the decision is made
//! from: the transaction records, the markers of where each part landed, the
//! intents, and the versions a reader passed over. They travel here as log
//! records with an across part, so a restore applies them through the same
//! settle a log apply runs, and the log above the cut then applies on top of
//! the restored store as it did on the source.
//!
//! # Order
//!
//! Records first, so a decision is known before a marker reconciles against
//! it; then the markers; then the held versions, oldest first, so a key's
//! versions keep their order and each lands above the version a reader sees.

use std::collections::{BTreeMap, VecDeque};

use tessari_encoding::{
    Across, AcrossPartKey, Decision, LogRecord, Mutation, Part, StoreKey, StoreValue,
    TransactionId, TransactionRecord, TransactionRecordKey,
};
use tessari_kv::{Key, KeyRange, ScanDirection, ScanRequest};
use tessari_types::{Reach, Sequence};

use crate::error::Result;
use crate::store::Store;

/// The part of a state that follows its records.
#[derive(Default)]
pub(super) struct Tail {
    /// Versions a reader at the snapshot passes over, and intents: each with
    /// the version it was written at on the source.
    held: Vec<(Sequence, Mutation)>,
    phase: Phase,
    /// The key the current phase's walk continues after.
    after: Option<Key>,
    /// Records ready to hand out, in order.
    queued: VecDeque<LogRecord>,
}

#[derive(Default, Clone, Copy)]
enum Phase {
    #[default]
    Records,
    Landed,
    Held,
    Done,
}

impl Tail {
    /// Keep a version the record walk does not hand out as the record.
    pub(super) fn hold(&mut self, at: Sequence, mutation: Mutation) {
        self.held.push((at, mutation));
    }

    /// The next record of the tail, or `None` once it is all out.
    pub(super) fn next(
        &mut self,
        store: &Store,
        version: Sequence,
        within: Reach,
    ) -> Result<Option<LogRecord>> {
        loop {
            if let Some(record) = self.queued.pop_front() {
                return Ok(Some(record));
            }
            match self.phase {
                Phase::Records => match self.step(
                    store,
                    TransactionRecordKey::keyspace(),
                    TransactionRecordKey::KIND.tag(),
                )? {
                    Some((key, value)) => {
                        let transaction = TransactionRecordKey::decode(key.as_slice())?.transaction;
                        let record = TransactionRecord::decode(value.as_slice())?;
                        let home = record.participants.first().map(|part| part.range);
                        if home.is_some_and(|home| reaches(within, home)) {
                            self.queue_decision(transaction, record);
                        }
                    }
                    None => self.next_phase(Phase::Landed),
                },
                Phase::Landed => {
                    match self.step(store, AcrossPartKey::keyspace(), AcrossPartKey::KIND.tag())? {
                        Some((key, value)) => {
                            let part = AcrossPartKey::decode(key.as_slice())?;
                            let landed = Sequence::decode(value.as_slice())?;
                            if landed <= version && reaches(within, part.range) {
                                self.queued
                                    .push_back(LogRecord::new(Vec::new()).across(Across {
                                        transaction: part.transaction,
                                        part: Part::Landed { range: part.range },
                                    }));
                            }
                        }
                        None => self.next_phase(Phase::Held),
                    }
                }
                Phase::Held => {
                    self.queued = held_records(std::mem::take(&mut self.held))?;
                    self.next_phase(Phase::Done);
                }
                Phase::Done => return Ok(None),
            }
        }
    }

    /// A restore writes a decided record the way the source did: `PENDING`
    /// first, since the record is written by compare-and-set and a decision
    /// may not be the first thing it holds.
    fn queue_decision(&mut self, transaction: TransactionId, record: TransactionRecord) {
        let decide = |record| {
            LogRecord::new(Vec::new()).across(Across {
                transaction,
                part: Part::Decide(record),
            })
        };
        if record.decision != Decision::Pending {
            self.queued.push_back(decide(TransactionRecord {
                decision: Decision::Pending,
                ..record.clone()
            }));
        }
        self.queued.push_back(decide(record));
    }

    fn next_phase(&mut self, phase: Phase) {
        self.phase = phase;
        self.after = None;
    }

    /// The next stored entry of the kind tagged `tag`, one at a time: a store
    /// keeps a marker per part of every transaction across leaders it ever
    /// held, which is not to be read into memory at once.
    fn step(
        &mut self,
        store: &Store,
        keyspace: tessari_kv::Keyspace,
        tag: u8,
    ) -> Result<Option<(Key, tessari_kv::Value)>> {
        let whole = KeyRange::prefix(&[tag]);
        let range = match &self.after {
            Some(last) => whole.resuming_after(last),
            None => whole,
        };
        let found = store
            .backend()
            .scan(&ScanRequest {
                keyspace,
                range,
                direction: ScanDirection::Forward,
                limit: Some(1),
            })?
            .into_iter()
            .next();
        if let Some((key, _)) = &found {
            self.after = Some(key.clone());
        }
        Ok(found)
    }
}

/// Whether a state read `within` carries what belongs to `range`.
fn reaches(within: Reach, range: Reach) -> bool {
    within == Reach::Store || within.contains(range)
}

/// The held versions as the records that wrote them: an intent is restored
/// by a prepare, which indexes it and marks where it landed; a resolved
/// version by a committed resolution, which derives what it derives. One
/// record per transaction, kind and range, in the order the source wrote
/// them.
fn held_records(mut held: Vec<(Sequence, Mutation)>) -> Result<VecDeque<LogRecord>> {
    held.sort_by_key(|(at, _)| *at);
    let mut groups: BTreeMap<(TransactionId, bool, Reach), (Sequence, Vec<Mutation>)> =
        BTreeMap::new();
    for (at, mutation) in held {
        let Some(provenance) = mutation.value.provenance() else {
            continue;
        };
        let range = crate::catalog::home_of(&LogRecord::new(vec![mutation.clone()]))?;
        let key = (provenance.transaction, provenance.provisional, range);
        groups
            .entry(key)
            .or_insert_with(|| (at, Vec::new()))
            .1
            .push(mutation);
    }
    let mut ordered: Vec<_> = groups.into_iter().collect();
    ordered.sort_by_key(|(_, (first, _))| *first);
    Ok(ordered
        .into_iter()
        .filter_map(|((transaction, provisional, _), (_, mutations))| {
            let coordinator = mutations
                .first()?
                .value
                .provenance()
                .map(|provenance| provenance.coordinator)?;
            let part = if provisional {
                Part::Prepare { coordinator }
            } else {
                Part::Resolve { committed: true }
            };
            Some(LogRecord::new(mutations).across(Across { transaction, part }))
        })
        .collect())
}
