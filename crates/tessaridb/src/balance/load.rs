//! Moving a placement because of measured load or a drained node — the
//! balancing round's fourth act, beside the count pass
//! ([`super::leaderships`]).
//!
//! # What triggers a move
//!
//! Two triggers, drain first:
//!
//! - **A drained node empties.** A voter whose roles the membership row
//!   cleared (`DEFINE NODE ROLES NONE`, the spec's *Draining this node*)
//!   leads nothing new: its placed range is a move candidate on the next
//!   passes regardless of load. The load signal cannot see a drain — the
//!   node's measured rate is on its way to zero, which reads as *quietest* —
//!   so the drain is its own branch, checked before any margin is.
//! - **Measured load.** Every rate the pass reads is one the split pass
//!   measured and published ([`SampledShard::writes_per_second`]) — the
//!   committer re-reads what its own pass counted, never a figure a peer
//!   self-reports, for the split pass's reason: a signal the committer cannot
//!   re-derive is not a signal. A move needs both:
//!   - the **margin** — the busiest voter's total measured load exceeds the
//!     quietest eligible voter's by the policy's factor; and
//!   - the **no-overshoot** bound — the moved range's own rate is at most half
//!     the gap, so the range lands on the quieter side without crossing it.
//!     Without this bound a hot range moved to an idle voter becomes the
//!     idle voter's overload and moves back next pass, one spacing window
//!     apart, forever — the flap the spacing exists to prevent, prevented at
//!     the decision instead.
//!
//! # What never triggers one
//!
//! The store line. A drained or hot store-line leader is the failover
//! policy's domain — the lease decides it, the act moves only what a peer row
//! places. And no policy row: the acts are OFF until a statement turns them
//! on, the way every automatic machinery here is.

use tessari_storage::{Catalog, NODE_ID_LEN, ReplicaDefinition, Roles, SampledTable};
use tessari_types::{Reach, TableId};

use super::leaderships::Line;
use super::{LeadershipMoves, Moved};
use crate::{Db, Result};

impl Db {
    /// Move one placement when the balancer policy asks, by load or by drain.
    ///
    /// The move is the one [`super::leaderships`] makes — the change
    /// `ALTER REPLICA to LEADS r; ALTER REPLICA from LEADS NONE` makes,
    /// committed in one storage transaction, the target first — so the two
    /// acts can never disagree about what a move is.
    ///
    /// # Errors
    ///
    /// The store's failure to read the catalog or commit, and the catalog's
    /// refusal of the move.
    pub async fn balance_load(&self, moves: &mut LeadershipMoves) -> Result<Option<Moved>> {
        let store = self.store();
        if !store.leads(Reach::Store).await? {
            return Ok(None);
        }
        let mut reading = store.begin().await?;
        let catalog = Catalog::new(&mut reading);
        // `None` is the ordinary answer on every node but the store line's
        // leader — and the designed-off answer everywhere while nobody has
        // set a policy: the acts are OFF until a statement turns them on.
        let Some(held) = catalog.balancer().await? else {
            reading.rollback();
            return Ok(None);
        };
        let rows = catalog.replicas().await?;
        reading.rollback();
        if moves.waits(held.policy.spacing()) {
            return Ok(None);
        }
        let lines = &store.line_leaders().await?;
        let sampled = store.sampled_shards();
        let Some(planned) = plan_load(&rows, lines, &held.policy, &sampled) else {
            return Ok(None);
        };
        let mut writing = store.begin().await?;
        let mut catalog = Catalog::new(&mut writing);
        let placed = catalog
            .alter_replica_leads(&planned.to, Some(planned.range), false)
            .await?;
        let released = catalog
            .alter_replica_leads(&planned.from, None, false)
            .await?;
        if !(placed && released) {
            writing.rollback();
            return Ok(None);
        }
        writing.commit().await?;
        store.leadership_moved_by_load();
        moves.last = Some(std::time::Instant::now());
        Ok(Some(planned))
    }
}

/// The move the policy asks for, if one is due: a drained voter's placement
/// first, otherwise the busiest measured voter's.
fn plan_load(
    rows: &[ReplicaDefinition],
    lines: &[Line],
    policy: &tessari_storage::Balancer,
    sampled: &[(TableId, SampledTable)],
) -> Option<Moved> {
    let rates = rates_of(sampled);
    let led = |node: [u8; NODE_ID_LEN]| {
        lines
            .iter()
            .filter(|(_, leader)| *leader == Some(node))
            .filter_map(|(range, _)| rates.get(range).copied())
            .sum::<u64>()
    };
    // Drain first: the load signal reads a draining node as quietest, which
    // is the direction it is being emptied in — and a node mid-drain waiting
    // on a margin would hold its leaderships past the drain's point.
    let drained = rows
        .iter()
        .find(|row| !row.roles.has(Roles::COORDINATING) && movable(row, lines));
    let (from, range) = if let Some(row) = drained {
        (row, row.leads?)
    } else {
        let (busiest, _) = rows
            .iter()
            .filter(|row| row.roles.has(Roles::COORDINATING))
            .filter_map(|row| Some((row, led(row.node?))))
            .max_by(|(left, many), (right, more)| {
                many.cmp(more).then(right.name.cmp(&left.name))
            })?;
        if !movable(busiest, lines) {
            return None;
        }
        (busiest, busiest.leads?)
    };
    let target = rows
        .iter()
        .filter(|row| row.roles.has(Roles::COORDINATING))
        .filter(|row| {
            row.leads.is_none() && row.replicates.is_some_and(|held| held.contains(range))
        })
        .filter_map(|row| Some((row, led(row.node?))))
        .min_by(|(left, quiet), (right, quieter)| {
            quiet.cmp(quieter).then(left.name.cmp(&right.name))
        })
        .map(|(row, _)| row)?;
    let quietest = led(target.node?);
    // The margin and the no-overshoot bound, in integer hundredths and whole
    // counts: floats would re-introduce the precision question the
    // fixed-point spelling exists to close. A drained source moves
    // regardless of either — emptiness is not a ratio.
    if drained.is_none() {
        // Checked, because a rate sum near `u64`'s ceiling is a number no
        // margin should be computed on — an overflow declines the move rather
        // than wrapping into one.
        let margin = u128::from(led(from.node?))
            .checked_mul(100)
            .zip(u128::from(policy.above_hundredths()).checked_mul(u128::from(quietest)))
            .is_some_and(|(left, right)| left > right);
        let carried = u128::from(rates_of_line(&rates, range));
        let gap = u128::from(led(from.node?)).saturating_sub(u128::from(quietest));
        if !margin || carried.saturating_mul(2) > gap {
            return None;
        }
    }
    Some(Moved {
        range,
        from: from.name.clone(),
        to: target.name.clone(),
    })
}

/// The measured rate of one line, zero when the pass has measured nothing
/// there — an unmeasured placement sheds on the margin alone, the way the
/// count pass moves what it cannot time.
fn rates_of_line(rates: &std::collections::BTreeMap<Reach, u64>, range: Reach) -> u64 {
    rates.get(&range).copied().unwrap_or(0)
}

/// Whether `row`'s placed range is one the pass may move: its own row's
/// placement, not releasing, not preferred, and it still leads the line —
/// the in-flight rule the count pass keeps.
fn movable(row: &ReplicaDefinition, lines: &[Line]) -> bool {
    !row.releasing
        && !row.preferred
        && match (row.leads, row.node) {
            (Some(range), Some(node)) => lines.contains(&(range, Some(node))),
            _ => false,
        }
}

/// The measured rate of each sampled shard, keyed by its line — the one map
/// the decision reads, built from what the split pass published and nothing
/// else.
fn rates_of(sampled: &[(TableId, SampledTable)]) -> std::collections::BTreeMap<Reach, u64> {
    let mut rates = std::collections::BTreeMap::new();
    for (table, held) in sampled {
        for shard in &held.shards {
            if let Some(rate) = shard.writes_per_second {
                rates.insert(
                    Reach::Shard(held.namespace, held.database, *table, shard.shard),
                    rate,
                );
            }
        }
    }
    rates
}

#[cfg(test)]
mod tests;
