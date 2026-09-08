//! Whether the index that won is better than reading the table.
//!
//! [`super::rank`] answers "which candidate narrows most", and until this
//! module existed that was the whole decision: any applicable index was taken,
//! because the scan was not a candidate and had no number to be compared
//! against. An ordered index that selects most of a table was therefore chosen
//! and was **2.1× slower than no index at all** — the entry walk and the record
//! fetch are both paid, and neither removes anything.
//!
//! The missing half is a ratio. A maintained per-table count alone cannot
//! decide it, because a range candidate has no ceiling to compare; a bounded
//! probe alone cannot decide it either, because a number of entries means
//! nothing without a table size to be a fraction of. Both are needed, which is
//! why [`crate::plan`] gained a store-reading guard rather than another rule in
//! the pure ranking.
//!
//! # The rule
//!
//! An index is served when it can produce **at most half** the table. Half
//! rather than "fewer than all", because an index read is not free: it walks
//! entries *and* fetches records, so a path that returns most of the table has
//! added a walk to a read it did not shorten. Half is a threshold and is stated
//! as one — it is not derived from a cost model, and this store deliberately
//! has no histograms (see [`crate::plan`] for why).
//!
//! # Why the probe is capped at the threshold itself
//!
//! Counting a range to the end costs what the range costs, which is the
//! expense the count exists to avoid. So the walk stops at the threshold: past
//! it, the only fact the caller needed is already known — this candidate is not
//! better than reading the table — and the walk that would have established
//! *how much* worse is never taken.
//!
//! # The guard does not engage on a small table
//!
//! Below [`PLANNER_SCAN_FLOOR_RECORDS`] the rule is switched off entirely. The
//! ratio it reasons about is a ratio between two costs that are both a single
//! round trip at that size, so there is no regression to prevent — and the one
//! thing the guard could still do is surprise somebody who declared an index on
//! a small table and watched it go unused. `USING INDEX <name>` would report
//! that out loud, correctly, and about nothing worth reporting.
//!
//! # What a missing number means
//!
//! `None` from the count is "no estimate", and a planner told nothing behaves
//! exactly as it did before this module existed: the index is served. That is
//! also what an unprobeable shape gets. A guard with no evidence never
//! overrules the ranking — it is a veto backed by a measurement, not a second
//! opinion.

use tessari_constants::PLANNER_SCAN_FLOOR_RECORDS;
use tessari_storage::{Catalog, Transaction};
use tessari_types::TableId;

use crate::error::Result;

use super::candidate::{Candidate, Rows, Served};

/// Whether serving `chosen` beats scanning `table`.
///
/// # Errors
///
/// Returns an error when the catalog or the index cannot be read.
pub(crate) fn worth_serving(
    transaction: &mut Transaction<'_>,
    table: TableId,
    chosen: &Candidate,
) -> Result<bool> {
    let Some(records) = Catalog::new(transaction).record_count(table)? else {
        return Ok(true);
    };
    if records < PLANNER_SCAN_FLOOR_RECORDS {
        return Ok(true);
    }
    let cap = records / 2;
    let ceiling = match (chosen.rows, &chosen.served) {
        // Already counted, and counted for free — a unique equality or a term's
        // document frequency. Nothing to probe.
        (Rows::AtMost(held), _) => Some(held),
        (Rows::Unknown, Served::Equality(values)) => {
            transaction.count_in_range(&chosen.index, values, None, None, cap)?
        }
        (
            Rows::Unknown,
            Served::Range {
                fixed,
                lower,
                upper,
            },
        ) => {
            transaction.count_in_range(&chosen.index, fixed, lower.as_ref(), upper.as_ref(), cap)?
        }
        // A string prefix, a search expansion and a region are each read
        // through a path of their own, so none of them is a range this probe
        // can walk. They keep the behaviour they had: the ranking decides and
        // the guard says nothing, because a guard that vetoed on no evidence
        // would take away a path that is often the right one.
        (Rows::Unknown, _) => return Ok(true),
    };
    Ok(match ceiling {
        Some(entries) => entries <= cap,
        // The probe gave up, which is the answer: more than half the table.
        None => false,
    })
}
