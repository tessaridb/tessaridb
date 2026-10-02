//! Turning what an index's statistics say into a number a candidate ranks by.
//!
//! # Where the numbers come from
//!
//! [`super::enumerate`] attaches the numbers that are free — a unique equality
//! names one record, a term's document frequency is a count of keys — and
//! leaves every other value candidate `Unknown`. A fresh statistic
//! (`ANALYZE TABLE`, or the serving node's own refresh) answers those: an
//! equality on a common value from its own count, on any other value from the
//! spread of the rest, a leading run of fields from its distinct count, a range
//! on the first field from the buckets it covers. A range under fixed leading
//! values, a prefix, a region and a search expansion get no estimate and keep
//! ranking by shape, as they did.
//!
//! # Scaled, and never trusted further than that
//!
//! A statistic counted the index when the table held some number of records;
//! the table's count now is kept exactly by every write, so the estimate is
//! scaled by the ratio. That corrects growth and shrinkage and nothing else —
//! a distribution that moved is what the change counter sets a statistic aside
//! for, and a set-aside statistic leaves the candidate `Unknown`.
//!
//! Nothing here changes an answer. An estimate picks between candidates and
//! lets the guard skip a count it would otherwise take; the records still come
//! from the index that was chosen, re-tested against the whole condition.

use tessari_storage::{Catalog, IndexDefinition, Transaction, estimate_equality, estimate_range};
use tessari_types::{TableId, Value};

use crate::error::Result;

use super::candidate::{Candidate, Rows, Served};
use super::rank::choose;
use super::reported::Plan;
use super::worth::worth_serving;

/// The candidate a read serves, if any beats reading the table — chosen with
/// estimates where the indexes have them, and carrying what is known about how
/// many records it produces.
///
/// The one function the read, `EXPLAIN` and each side of a union ask, so the
/// three cannot come to disagree about which index runs.
///
/// # Errors
///
/// Returns an error when the catalog, a statistic or an index cannot be read.
pub(crate) fn serving(
    transaction: &mut Transaction<'_>,
    table: TableId,
    offered: Vec<Candidate>,
    lifted: bool,
) -> Result<Option<Candidate>> {
    let offered = estimated(transaction, table, offered)?;
    let Some(mut chosen) = choose(offered) else {
        return Ok(None);
    };
    match worth_serving(transaction, table, &chosen, lifted)? {
        Some(rows) => {
            chosen.rows = rows;
            Ok(Some(chosen))
        }
        None => Ok(None),
    }
}

/// An index range as a read walks it: the index, its fixed leading values and
/// the bounds on the field after them.
pub(crate) type Ranged<'a> = (
    &'a IndexDefinition,
    &'a [Value],
    Option<&'a Value>,
    Option<&'a Value>,
);

/// Whether the index a filtered nearest read's condition chose narrows the read
/// to no more records than its walk would visit.
///
/// Asked by the read and by `EXPLAIN` from the same plan, so the two agree on
/// whether such a read walks the graph or reads the condition's index exactly.
/// A number the plan already carries — a free ceiling, a statistic, a probe's
/// count — decides; without one an index range is counted, stopping at the
/// ceiling, which costs a bounded walk of keys and never the records. A shape
/// with no number and no range to count walks, as it did.
///
/// # Errors
///
/// Returns an error when the index cannot be read.
pub(crate) fn narrows_to(
    transaction: &Transaction<'_>,
    plan: &Plan,
    range: Option<Ranged<'_>>,
    ceiling: usize,
) -> Result<bool> {
    let ceiling = u64::try_from(ceiling).unwrap_or(u64::MAX);
    if let Some(expected) = plan.expected {
        return Ok(expected.rows() <= ceiling);
    }
    Ok(match range {
        Some((index, fixed, lower, upper)) => transaction
            .count_in_range(index, fixed, lower, upper, ceiling)?
            .is_some(),
        None => false,
    })
}

/// Every `Unknown` candidate a fresh statistic can estimate, estimated.
fn estimated(
    transaction: &mut Transaction<'_>,
    table: TableId,
    mut offered: Vec<Candidate>,
) -> Result<Vec<Candidate>> {
    if !offered
        .iter()
        .any(|candidate| candidate.rows == Rows::Unknown)
    {
        return Ok(offered);
    }
    let records = Catalog::new(transaction).record_count(table)?;
    for candidate in &mut offered {
        if candidate.rows != Rows::Unknown {
            continue;
        }
        let Some(statistics) = transaction.fresh_statistics(&candidate.index)? else {
            continue;
        };
        let counted = match &candidate.served {
            Served::Equality(values) => estimate_equality(&statistics, values),
            Served::Range {
                fixed,
                lower,
                upper,
            } if fixed.is_empty() => estimate_range(&statistics, lower.as_ref(), upper.as_ref()),
            _ => continue,
        };
        candidate.rows = Rows::About(scaled(counted, records, statistics.records));
    }
    Ok(offered)
}

/// `counted × now / then`, the estimate carried to the table's present size.
fn scaled(counted: u64, now: Option<u64>, then: u64) -> u64 {
    let Some(now) = now.filter(|_| then > 0) else {
        return counted;
    };
    let scaled = u128::from(counted)
        .saturating_mul(u128::from(now))
        .checked_div(u128::from(then))
        .unwrap_or(0);
    u64::try_from(scaled).unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use super::scaled;

    #[test]
    fn an_estimate_follows_the_table_as_it_grows_and_shrinks() {
        assert_eq!(scaled(1_000, Some(20_000), 10_000), 2_000);
        assert_eq!(scaled(1_000, Some(5_000), 10_000), 500);
        assert_eq!(scaled(1_000, None, 10_000), 1_000);
        assert_eq!(scaled(1_000, Some(5_000), 0), 1_000);
    }
}
