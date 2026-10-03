//! Moving a placement off a node that leads more lines than a voter beside it
//! (ADR-0113 D3).
//!
//! # What is counted
//!
//! Lines, not placed ranges: a replica row places one range, so a node leads
//! at most one placed range, and placed ranges alone never differ by more than
//! one. The imbalance that does arise is the store line's leader also leading
//! the range its own row places — after a failover, or a move — while another
//! voter leads nothing. Each line's leader is the one the write gate names
//! ([`tessari_storage::Store::line_leaders`]).
//!
//! # When nothing is done
//!
//! Only the busiest node's own placement is ever moved, and only while it
//! leads it: a node still leading a range it was moved off is a move in
//! flight, and its count drops when that lands. And never twice within two
//! lease periods, the hand-over bound (ADR-0098 D2) with room for the election
//! that follows it.

use std::time::Instant;

use tessari_storage::{Catalog, NODE_ID_LEN, ReplicaDefinition, Roles};
use tessari_types::Reach;

use crate::{Db, Result};

/// When this node last moved a placement, to space the next move.
///
/// Owned by the caller's loop rather than by the node, as
/// [`crate::ShardSamples`] is: one balancer runs per process.
#[derive(Debug, Default)]
pub struct LeadershipMoves {
    last: Option<Instant>,
}

impl LeadershipMoves {
    /// Whether the last move was less than `spacing` ago.
    fn waits(&self, spacing: std::time::Duration) -> bool {
        self.last.is_some_and(|last| last.elapsed() < spacing)
    }
}

/// One placement moved: `range` from the row `from` to the row `to`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Moved {
    /// The range whose placement moved.
    pub range: Reach,
    /// The row that placed it, and leads one line fewer once the move lands.
    pub from: String,
    /// The row that places it now.
    pub to: String,
}

type Line = (Reach, Option<[u8; NODE_ID_LEN]>);

impl Db {
    /// Move one placement when the failover policy asks for balanced
    /// leaderships, this node may commit to the store line, and one node
    /// leads more than one line more than a voter that could take its range.
    ///
    /// The move is the change `ALTER REPLICA to LEADS r; ALTER REPLICA from
    /// LEADS NONE` makes, committed in one storage transaction through the same
    /// catalog calls and write gate — the target first, so the range never has
    /// no candidate.
    ///
    /// # Errors
    ///
    /// The store's failure to read the catalog or commit, and the catalog's
    /// refusal of the move.
    pub fn balance_leaderships(&self, moves: &mut LeadershipMoves) -> Result<Option<Moved>> {
        let store = self.store();
        if !store.leads(Reach::Store)? {
            return Ok(None);
        }
        let mut reading = store.begin()?;
        let catalog = Catalog::new(&mut reading);
        let policy = catalog.failover()?;
        let rows = catalog.replicas()?;
        reading.rollback();
        let Some(policy) = policy.filter(|held| held.balance_leaderships) else {
            return Ok(None);
        };
        let spacing = policy.policy.lease().saturating_mul(2);
        if moves.waits(spacing) {
            return Ok(None);
        }
        let Some(planned) = plan(&rows, &store.line_leaders()?) else {
            return Ok(None);
        };
        let mut writing = store.begin()?;
        let mut catalog = Catalog::new(&mut writing);
        let placed = catalog.alter_replica_leads(&planned.to, Some(planned.range), false)?;
        let released = catalog.alter_replica_leads(&planned.from, None, false)?;
        if !(placed && released) {
            writing.rollback();
            return Ok(None);
        }
        writing.commit()?;
        moves.last = Some(Instant::now());
        Ok(Some(planned))
    }
}

/// The move that evens out the lines led, if one is due.
fn plan(rows: &[ReplicaDefinition], lines: &[Line]) -> Option<Moved> {
    let led = |node: [u8; NODE_ID_LEN]| {
        lines
            .iter()
            .filter(|(_, leader)| *leader == Some(node))
            .count()
    };
    let voters: Vec<(&ReplicaDefinition, usize)> = rows
        .iter()
        .filter(|row| row.roles.has(Roles::COORDINATING))
        .filter_map(|row| Some((row, led(row.node?))))
        .collect();
    let (busiest, most) = voters
        .iter()
        .max_by(|(left, many), (right, more)| many.cmp(more).then(right.name.cmp(&left.name)))?;
    // The busiest node must lead the range its own row places: one still
    // leading a range it was moved off is a move in flight, and moving the
    // placement it holds now would not lower what it leads.
    let range = busiest.leads?;
    // A placement being given back to the store line, or one marked
    // preferred, is the operator's decision about that range; moving it to a
    // voter would overrule it.
    if busiest.releasing || busiest.preferred || !lines.contains(&(range, busiest.node)) {
        return None;
    }
    let (target, fewest) = voters
        .iter()
        .filter(|(row, _)| {
            row.leads.is_none() && row.replicates.is_some_and(|held| held.contains(range))
        })
        .min_by(|(left, few), (right, fewer)| few.cmp(fewer).then(left.name.cmp(&right.name)))?;
    (most.saturating_sub(*fewest) > 1).then(|| Moved {
        range,
        from: busiest.name.clone(),
        to: target.name.clone(),
    })
}

#[cfg(test)]
mod tests;
