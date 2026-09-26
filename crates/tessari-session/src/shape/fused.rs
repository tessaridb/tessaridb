//! Several orders fused into one by rank (`ORDER BY FUSE`, G038).
//!
//! # Why ranks and not the values
//!
//! A relevance score and a distance are on unrelated scales: added together,
//! whichever has the larger range on this table decides the order, and the
//! weight a caller wrote is not the weight that applies. A rank is on one scale
//! whatever produced it. So each branch orders every record on its own — by
//! exactly the rule a single `ORDER BY` on that key uses, ties by identity — and
//! a record earns `weight / (K + rank)` from each branch it is within the first
//! `depth` of. The sum orders the answer; equal sums fall back to identity, the
//! rule every order in this store ends with.
//!
//! # Why it holds every record
//!
//! A record's rank in one branch depends on every other record, so nothing can
//! be discarded until every branch has seen them all. The bounded collector's
//! compaction is sound for one total order and wrong here, which is why a fused
//! read never takes it.

use core::cmp::Ordering as Order;

use tessari_constants::{FUSION_DEPTH, FUSION_K};
use tessari_ql::{Fusion, Ordering};
use tessari_types::{RecordId, Value};

use super::{Keyed, ranked};

/// A record of a fused answer, with its rank in each branch — `None` where it
/// fell outside that branch's depth.
pub(crate) type Fused = (RecordId, Value, Vec<Option<u64>>);

/// `held`, fused by the branches in `order` under `fusion`, best first.
///
/// A record within no branch's depth is not in the answer: nothing ranked it.
pub(crate) fn fused(held: Vec<Keyed>, order: &[Ordering], fusion: &Fusion) -> Vec<Fused> {
    let depth = fusion.depth.unwrap_or(FUSION_DEPTH);
    let mut ranks: Vec<Vec<Option<u64>>> = vec![vec![None; order.len()]; held.len()];
    for (branch, key) in order.iter().enumerate() {
        let mut by_branch: Vec<usize> = (0..held.len()).collect();
        by_branch.sort_by(|left, right| branch_order(&held, *left, *right, branch, key));
        for (place, index) in by_branch.into_iter().enumerate() {
            let rank = u64::try_from(place).map_or(u64::MAX, |place| place.saturating_add(1));
            if rank > depth {
                break;
            }
            if let Some(slot) = ranks.get_mut(index).and_then(|row| row.get_mut(branch)) {
                *slot = Some(rank);
            }
        }
    }
    let mut scored: Vec<(f64, Keyed, Vec<Option<u64>>)> = held
        .into_iter()
        .zip(ranks)
        .filter(|(_, row)| row.iter().any(Option::is_some))
        .map(|(record, row)| (score(&row, &fusion.weights), record, row))
        .collect();
    scored.sort_by(|left, right| {
        right
            .0
            .total_cmp(&left.0)
            .then_with(|| left.1.1.cmp(&right.1.1))
    });
    scored
        .into_iter()
        .map(|(_, (_, id, record), row)| (id, record, row))
        .collect()
}

/// Two records by one branch's key alone, ties by identity — the order a single
/// `ORDER BY` on that key gives.
fn branch_order(held: &[Keyed], left: usize, right: usize, branch: usize, key: &Ordering) -> Order {
    let (Some(left), Some(right)) = (held.get(left), held.get(right)) else {
        return Order::Equal;
    };
    // One key each, borrowed as a one-element slice: the comparison runs
    // `n log n` times per branch, and a copy per comparison would be the cost.
    fn pick(keyed: &Keyed, branch: usize) -> &[Value] {
        keyed.0.get(branch).map_or(&[][..], core::slice::from_ref)
    }
    ranked(
        (pick(left, branch), &left.1),
        (pick(right, branch), &right.1),
        core::slice::from_ref(key),
    )
}

/// `Σ weight / (K + rank)` over the branches that ranked the record.
fn score(row: &[Option<u64>], weights: &[tessari_types::Number]) -> f64 {
    let k = whole(FUSION_K);
    row.iter()
        .zip(weights)
        .filter_map(|(rank, weight)| Some(weight.as_float()? / (k + whole((*rank)?))))
        .sum()
}

/// A count as a float, exact below 2^53 and saturating far past any depth.
fn whole(count: u64) -> f64 {
    u32::try_from(count).map_or(f64::from(u32::MAX), f64::from)
}
