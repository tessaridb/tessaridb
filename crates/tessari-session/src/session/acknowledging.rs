//! A write that waits until enough copies hold it (ADR-0106).
//!
//! # Decided before the commit, waited for after it
//!
//! Which level a write waits for, and whether that level can be met at all, are
//! both decided BEFORE anything is written: a majority the declared voters
//! cannot form is refused with nothing committed (D7), and so is a request for
//! less than its namespace allows (D2). Only then does the write commit — it is
//! visible on the leader from that moment, as in every database this follows
//! (D3) — and the caller's answer waits for the copies. A wait that runs out is
//! refused by name and says the write IS committed here (D4): the alternative,
//! answering success as though the copies existed, is the silent loss this
//! exists to end.
//!
//! # Majority of whom
//!
//! Of the range's VOTERS — this node and every peer declared `coordinating` —
//! because they are the set whose majority elects the next leader, and a write
//! a majority of them holds is held by every majority that can elect one (D8).
//! A voter whose subscription does not cover the write can never hold it, so it
//! never counts; when the holders cannot make up a majority the write is
//! refused before it commits rather than accepted and timed out.
//!
//! # A local majority
//!
//! `LOCAL MAJORITY` counts the same way over the voters in this node's region
//! — its own row's `REGION` — and so waits only for copies that need not cross
//! a region (G057 C3). It is weaker than `MAJORITY` in one stated way: the
//! majority that elects the next leader need not include a node of this region
//! holding the write, so a failover is not promised to keep it.

use tessari_ql::Span;
use tessari_storage::{Catalog, Failover, NODE_ID_LEN, Reach, Roles, Store, Transaction};
use tessari_types::{Acknowledge, Replication};

use super::{Session, advised};
use crate::error::{Error, Result};

/// What a commit waits for once it has landed.
#[derive(Debug)]
pub(super) struct Waiting {
    /// The voters, other than this node, whose copy can hold the write.
    holders: Vec<([u8; NODE_ID_LEN], String)>,
    /// How many of them must hold it: a majority of the voters, less this node.
    needed: usize,
    /// How long the caller waits for them — the failover policy's round.
    within: std::time::Duration,
}

impl Session<'_> {
    /// Decide what this commit waits for, refusing now if it cannot be met.
    ///
    /// Judged against the range the commit LANDS in, not the namespace the
    /// session has selected: a membership row is a store-wide write whatever
    /// `USE` came before it, and a write lands where its records live.
    pub(super) fn acknowledgement_for(
        &self,
        transaction: &mut Transaction<'_>,
        asked: Option<Acknowledge>,
        span: Span,
    ) -> Result<Option<Waiting>> {
        // A read commits an empty transaction, and waits for nobody.
        if !transaction.writes_anything() {
            return Ok(None);
        }
        self.acknowledgement_in(transaction, None, asked, span)
    }

    /// [`Self::acknowledgement_for`], for a commit landing in `home` — or, when
    /// `home` is `None`, wherever the transaction's own writes land.
    ///
    /// A record of a transaction across leaders names its range itself: a
    /// decision writes no records, so nothing else could say where it lands
    /// (ADR-0112).
    pub(super) fn acknowledgement_in(
        &self,
        transaction: &mut Transaction<'_>,
        home: Option<Reach>,
        asked: Option<Acknowledge>,
        span: Span,
    ) -> Result<Option<Waiting>> {
        let me = self.store.node_identity()?.id;
        let voting: Vec<_> = Catalog::new(transaction)
            .replicas()?
            .into_iter()
            .filter(|peer| peer.roles.has(Roles::COORDINATING))
            .filter_map(|peer| peer.node.map(|node| (node, peer)))
            .filter(|(node, _)| *node != me)
            .collect();
        // A store with no voters but itself is its own majority, and a request
        // that named no level has nothing to be checked against: neither pays
        // for working out where the write lands.
        if voting.is_empty() && asked.is_none() {
            return Ok(None);
        }
        let home = match home {
            Some(home) => home,
            None => transaction.home()?,
        };
        let catalog = Catalog::new(transaction);
        let definition = match home {
            Reach::Store => None,
            Reach::Namespace(namespace)
            | Reach::Database(namespace, _)
            | Reach::Shard(namespace, ..) => catalog.namespace(namespace)?,
        };
        let stated = definition.as_ref().and_then(|held| held.acknowledge);
        if let (Some(asked), Some(stated), Some(held)) = (asked, stated, definition.as_ref())
            && !stated.admits(asked)
        {
            return Err(Error::AcknowledgeBelowNamespace {
                asked,
                stated,
                namespace: held.name.clone(),
                span,
            });
        }
        // Unstated, the level follows the replication: a namespace kept on more
        // than one node waits for a majority of them, one kept on one node is
        // its own majority (D1). A store-wide write waits only when asked to.
        let replicated = definition
            .as_ref()
            .and_then(|held| held.replication)
            .is_some_and(|replication| match replication {
                Replication::Factor(factor) => factor.get() > 1,
                Replication::None => false,
            });
        let level = asked
            .or(stated.map(|stated| stated.level))
            .unwrap_or(if replicated {
                Acknowledge::Majority
            } else {
                Acknowledge::Leader
            });
        let voting = if level == Acknowledge::LocalMajority {
            let mine = Catalog::new(transaction)
                .replicas()?
                .into_iter()
                .find(|row| row.node == Some(me))
                .and_then(|row| row.region)
                .ok_or(Error::LocalMajorityWithoutRegion { span })?;
            voting
                .into_iter()
                .filter(|(_, peer)| peer.region.as_deref() == Some(mine.as_str()))
                .collect()
        } else {
            voting
        };
        let catalog = Catalog::new(transaction);
        let needed = voting.len().saturating_add(1).div_euclid(2);
        if level == Acknowledge::Leader || needed == 0 {
            return Ok(None);
        }
        let holders: Vec<_> = voting
            .iter()
            .filter(|(_, peer)| peer.replicates.is_some_and(|over| over.contains(home)))
            .map(|(node, peer)| (*node, peer.name.clone()))
            .collect();
        if holders.len() < needed {
            return Err(Error::MajorityUnreachable {
                holders: holders.into_iter().map(|(_, name)| name).collect(),
                voters: voting.len().saturating_add(1),
                needed: needed.saturating_add(1),
                span,
            });
        }
        let within = catalog
            .failover()?
            .map_or(Failover::DEFAULT, |definition| definition.policy)
            .round();
        Ok(Some(Waiting {
            holders,
            needed,
            within,
        }))
    }

    /// Commit, then wait for the copies `waiting` names.
    pub(super) fn commit_acknowledged(
        store: &Store,
        transaction: Transaction<'_>,
        waiting: Option<Waiting>,
        span: Span,
    ) -> Result<()> {
        let committed = transaction.commit_placed().map_err(advised)?;
        Self::await_acknowledged(store, committed, waiting, span)
    }

    /// Wait for the copies `waiting` names to hold a commit that has landed.
    pub(super) fn await_acknowledged(
        store: &Store,
        committed: tessari_storage::Committed,
        waiting: Option<Waiting>,
        span: Span,
    ) -> Result<()> {
        let Some(waiting) = waiting else {
            return Ok(());
        };
        let nodes: Vec<[u8; NODE_ID_LEN]> = waiting.holders.iter().map(|(node, _)| *node).collect();
        let began = std::time::Instant::now();
        let held = store.await_held(
            committed.log,
            committed.sequence,
            &nodes,
            waiting.needed,
            waiting.within,
        );
        let confirmed = held.len() >= waiting.needed;
        store.acknowledgement_waited(began.elapsed(), !confirmed);
        let waited_us = u64::try_from(began.elapsed().as_micros()).unwrap_or(u64::MAX);
        tracing::debug!(
            home = ?committed.log.home,
            waited_us,
            "a commit waited for its acknowledgement"
        );
        if confirmed {
            return Ok(());
        }
        Err(Error::NotAcknowledgedInTime {
            sequence: committed.sequence.get(),
            held_by: waiting
                .holders
                .into_iter()
                .filter(|(node, _)| held.contains(node))
                .map(|(_, name)| name)
                .collect(),
            needed: waiting.needed.saturating_add(1),
            span,
        })
    }
}
