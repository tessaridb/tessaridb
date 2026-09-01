//! What refining a spatial index's candidates costs, measured at its build.
//!
//! A spatial index filters by **bounding box** and the exact predicate decides,
//! so the index's own health is the ratio between what it offers and what
//! survives. This module computes that ratio the one moment it can be computed
//! honestly, and the ratio is then reported by `INFO FOR GEO`.

use std::collections::BTreeMap;

use tessari_constants::SPATIAL_QUERY_CELLS;
use tessari_encoding::{
    IndexAddress, SpatialRefinement, SpatialRefinementKey, StoreKey, StoreValue,
};
use tessari_geo::{Bounds, Cell, Relation};
use tessari_kv::WriteBatch;
use tessari_types::RecordId;

/// How many records a measurement queries with.
///
/// The same thirty-two the vector recall averages over, chosen the same way and
/// for the same trade: large enough that one unlucky query cannot carry the
/// figure, small enough to be a fraction of the build it rides on rather than a
/// new expense.
const MEASURED_SAMPLE: usize = 32;

/// The relation a measuring query asks for.
///
/// A window query — "what is in this box" — is what a spatial index is read for,
/// and it is the relation whose candidate set is largest, so it is the one that
/// exposes a loose covering. A narrower relation would measure a filter that is
/// mostly the predicate's work rather than the index's.
/// A read asking a narrower relation refines a smaller set, so a figure taken
/// under this one does not describe that read's cost. That makes the relation
/// part of the measurement rather than an implementation detail, which is why
/// it is public: `INFO FOR GEO` reports it beside the ratio.
pub const MEASURED_RELATION: Relation = Relation::Meets;

/// One record as the index holds it: where it is, and which cells say so.
#[derive(Debug, Clone)]
pub(crate) struct Placed {
    /// The record.
    pub id: RecordId,
    /// The box every one of its entries carries.
    pub bounds: Bounds,
    /// The cells its covering placed it in.
    pub cells: Vec<Cell>,
}

/// Write this index's measured refinement into the batch, if it has one.
///
/// Called from a build, where every row's geometry and covering are already in
/// hand, so the measurement costs cell arithmetic and no reads. A build clears
/// the index's keyspace first, so an index with nothing to measure leaves **no**
/// key rather than a stale one — and absence is what `INFO FOR GEO` reports as
/// never measured.
pub(crate) fn measure(batch: WriteBatch, address: &IndexAddress, placed: &[Placed]) -> WriteBatch {
    let Some(measured) = refinement(placed) else {
        return batch;
    };
    batch.put(
        SpatialRefinementKey::keyspace(),
        SpatialRefinementKey::new(*address).encode(),
        measured.encode(),
    )
}

/// The counts a sample of this index's own records produces as queries.
///
/// `None` when there is nothing to measure — fewer than two records, or a
/// sample in which no query ever **reached** another record. The second case is
/// not a degenerate one to be papered over: a store whose geometries are far
/// enough apart that no box reaches a neighbour has no refinement cost, and
/// reporting a figure for it would be reporting a number nobody computed.
///
/// The bar is `reached`, deliberately, and it was `admitted` for one draft.
/// Suppressing a measurement in which records were reached and **none** were
/// admitted would hide the single worst thing a covering can do — offer
/// candidates the box test throws away entirely — which is precisely what this
/// measurement exists to make visible. That case is measured, stored, and left
/// for the ratio to report as unbounded rather than as zero.
fn refinement(placed: &[Placed]) -> Option<SpatialRefinement> {
    let records = placed.len();
    if records < 2 {
        return None;
    }
    let stride = records.div_ceil(MEASURED_SAMPLE).max(1);
    let mut entries = 0_u64;
    let mut reached = 0_u64;
    let mut admitted = 0_u64;
    let mut sample = 0_u32;

    for asking in placed.iter().step_by(stride) {
        let query = tessari_geo::covering(asking.bounds, SPATIAL_QUERY_CELLS);
        // Whether each record reached survived its own box test, judged once per
        // record however many cells reached it — every entry of a record carries
        // the record's own box, so a second reading cannot differ from the first.
        let mut judged: BTreeMap<&RecordId, bool> = BTreeMap::new();
        for held in placed {
            // The record being asked about is always reached by its own box and
            // always survives it. Counting it would add one to both sides of
            // every ratio and pull each one toward one — toward "healthy".
            if held.id == asking.id {
                continue;
            }
            for (theirs, ours) in held
                .cells
                .iter()
                .flat_map(|theirs| query.iter().map(move |(ours, _)| (theirs, ours)))
            {
                if !on_one_path(*theirs, *ours) {
                    continue;
                }
                entries = entries.saturating_add(1);
                judged
                    .entry(&held.id)
                    .or_insert_with(|| MEASURED_RELATION.admits(asking.bounds, held.bounds));
            }
        }
        reached = reached.saturating_add(count(judged.len()));
        admitted = admitted.saturating_add(count(judged.values().filter(|kept| **kept).count()));
        sample = sample.saturating_add(1);
    }

    if reached == 0 {
        return None;
    }
    Some(SpatialRefinement {
        entries,
        reached,
        admitted,
        sample,
        records: count(records),
    })
}

/// Whether a stored cell is reached by a query cell.
///
/// The two halves of the read path's traversal, as one test on the cells rather
/// than as two scans over keys:
///
/// - a stored cell **at or below** the query cell, which the forward scan of the
///   query cell's descendants finds;
/// - a stored cell **above** it, which is where a record larger than the query
///   box sits and which only the per-level lookup reaches.
///
/// Both are the same statement: one cell is an ancestor-or-self of the other, so
/// they agree at the shallower of their two levels. Dropping the second half here
/// would produce a figure describing a read path this store does not have.
fn on_one_path(one: Cell, other: Cell) -> bool {
    let shallow = one.level().min(other.level());
    one.ancestor(shallow) == other.ancestor(shallow)
}

/// A count as a `u64`, without an `as` cast.
fn count(held: usize) -> u64 {
    u64::try_from(held).unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::panic, clippy::unwrap_used)]

    use super::{Placed, on_one_path, refinement};
    use tessari_geo::{Bounds, Cell, Snapped};
    use tessari_types::RecordId;

    fn position(longitude: i64, latitude: i64) -> Snapped {
        Snapped::from_units(longitude, latitude).expect("a position on the grid")
    }

    /// A degenerate box at one position — what a point record stores.
    fn at(longitude: i64, latitude: i64) -> Bounds {
        Bounds::of_position(position(longitude, latitude))
    }

    /// A box with extent, which is what any shape larger than a point stores.
    fn region(west: i64, south: i64, east: i64, north: i64) -> Bounds {
        Bounds::of_position(position(west, south)).widened_to(position(east, north))
    }

    fn placed(n: i64, bounds: Bounds) -> Placed {
        let cells =
            tessari_geo::covering(bounds, tessari_constants::SPATIAL_INDEX_CELLS_PER_RECORD)
                .into_iter()
                .map(|(cell, _)| cell)
                .collect();
        Placed {
            id: RecordId::Int(n),
            bounds,
            cells,
        }
    }

    #[test]
    fn a_cell_is_on_its_own_path() {
        let root = Cell::root();
        assert!(on_one_path(root, root));
    }

    #[test]
    fn a_coarser_cell_reaches_a_finer_one_and_the_reverse() {
        let fine = Cell::containing(position(1_000, 2_000));
        let coarse = fine
            .ancestor(fine.level().saturating_sub(4))
            .expect("above");
        // Both directions, because a record larger than the query box is reached
        // by the second half of the traversal and a smaller one by the first. A
        // rule that held only one way would lose exactly the large records.
        assert!(on_one_path(fine, coarse));
        assert!(on_one_path(coarse, fine));
    }

    #[test]
    fn two_cells_in_different_parts_of_the_world_are_not_on_one_path() {
        let here = Cell::containing(position(1_000, 2_000));
        let far = Cell::containing(position(-1_000_000_000, -900_000_000));
        assert!(!on_one_path(here, far));
    }

    /// Whether the read path's own key spans reach a stored cell from a query
    /// cell — built from the same two constructors `records_in_region` uses.
    ///
    /// This is the oracle for [`on_one_path`]. The measurement re-states the
    /// traversal's semantics in cell arithmetic instead of key ranges, which is
    /// what makes it cheap and also what makes it able to drift; a figure
    /// derived from a reach rule the store does not have would describe a read
    /// path nobody can run. So the rule is asserted against the spans rather
    /// than against a second reading of itself.
    fn reached_by_the_key_spans(stored: Cell, query: Cell) -> bool {
        use std::ops::Bound;
        use tessari_encoding::{IndexAddress, SpatialIndexKey, StoreKey};
        use tessari_kv::{Key, KeyRange};
        use tessari_types::{DatabaseId, IndexId, NamespaceId, TableId};

        fn within(span: &KeyRange, key: &Key) -> bool {
            let after_start = match span.start() {
                Bound::Included(from) => key.as_slice() >= from.as_slice(),
                Bound::Excluded(from) => key.as_slice() > from.as_slice(),
                Bound::Unbounded => true,
            };
            let before_end = match span.end() {
                Bound::Included(to) => key.as_slice() <= to.as_slice(),
                Bound::Excluded(to) => key.as_slice() < to.as_slice(),
                Bound::Unbounded => true,
            };
            after_start && before_end
        }

        let address = IndexAddress::new(
            NamespaceId::new(1),
            DatabaseId::new(2),
            TableId::new(3),
            IndexId::new(4),
        );
        let key = SpatialIndexKey::new(address, stored, RecordId::Int(1)).encode();
        let mut spans = vec![SpatialIndexKey::descendants(&address, query)];
        for level in 0..query.level() {
            if let Some(above) = query.ancestor(level) {
                spans.push(KeyRange::prefix(&SpatialIndexKey::cell_prefix(
                    &address, above,
                )));
            }
        }
        spans.iter().any(|span| within(span, &key))
    }

    #[test]
    fn the_reach_rule_agrees_with_the_key_spans_the_read_path_scans() {
        // The drift test. Every pair of cells drawn from four positions across
        // the world at seven levels each: the rule and the spans must agree on
        // all of them, or the measured figure describes a traversal this store
        // does not perform.
        let corners = [
            position(1_000, 2_000),
            position(-1_000_000_000, -900_000_000),
            position(170_000_000_000, 80_000_000_000),
            position(0, 0),
        ];
        let finest = Cell::containing(corners[0]).level();
        let mut cells = Vec::new();
        for corner in corners {
            let leaf = Cell::containing(corner);
            for step in [0, 1, 4, 8, 16, 24, finest] {
                if let Some(cell) = leaf.ancestor(step.min(finest)) {
                    cells.push(cell);
                }
            }
        }
        let mut agreed_true = 0_usize;
        let mut agreed_false = 0_usize;
        for stored in &cells {
            for query in &cells {
                let by_rule = on_one_path(*stored, *query);
                let by_spans = reached_by_the_key_spans(*stored, *query);
                assert_eq!(
                    by_rule, by_spans,
                    "stored {stored:?} query {query:?}: rule {by_rule}, spans {by_spans}"
                );
                if by_rule {
                    agreed_true = agreed_true.saturating_add(1);
                } else {
                    agreed_false = agreed_false.saturating_add(1);
                }
            }
        }
        // Both verdicts must actually occur, or the agreement above is the
        // agreement of two functions that always say the same thing.
        assert!(agreed_true > 0, "no pair was ever reached");
        assert!(agreed_false > 0, "every pair was reached");
    }

    #[test]
    fn one_record_has_nothing_to_measure_against() {
        // Not zero and not a perfect score: there is no other record for a query
        // to reach, so no query was ever answered.
        assert!(refinement(&[placed(1, at(1_000, 2_000))]).is_none());
        assert!(refinement(&[]).is_none());
    }

    #[test]
    fn records_that_never_reach_one_another_measure_nothing() {
        // The decisive input. Two points on opposite sides of the world share no
        // cell at any level, so each query reaches nobody, `admitted` is zero and
        // the measurement writes nothing at all. A store like this must not
        // report a ratio, because the honest answer is that none was computed.
        let far_apart = [
            placed(1, at(1_000, 2_000)),
            placed(2, at(-1_000_000_000, -900_000_000)),
        ];
        assert!(refinement(&far_apart).is_none());
    }

    #[test]
    fn neighbours_are_reached_and_admitted_and_the_asking_record_is_not() {
        // Two points close enough to share a cell. Each queries with its own box
        // and reaches exactly the other — one, not two — which is what excluding
        // the asking record buys, and what keeps the ratio from being pulled
        // toward one by a record's guaranteed hit on itself.
        let close = [placed(1, at(1_000, 2_000)), placed(2, at(1_001, 2_001))];
        let measured = refinement(&close).expect("two neighbours measure");
        assert_eq!(measured.sample, 2, "both records asked");
        assert_eq!(measured.records, 2);
        assert_eq!(measured.reached, 2, "one other record per query");
        // Degenerate boxes at distinct positions do not meet, so the box test
        // admits neither. This is a measurement and a damning one — the covering
        // offered a record and the boxes kept none of it — and it is exactly the
        // case an earlier draft suppressed by testing `admitted` instead of
        // `reached`. Keeping the two counts apart is what makes it sayable.
        assert_eq!(measured.admitted, 0);
        // Unbounded, not zero: a zero here would read as a filter that wastes
        // nothing, which is the opposite of what happened.
        assert_eq!(measured.refinement(), None);
    }

    #[test]
    fn a_record_inside_anothers_box_is_reached_and_kept() {
        // A box with extent, holding a point. The point's query box meets the
        // region's, so the region is both reached and admitted — the one shape
        // where a stored record survives refinement rather than being filtered.
        let corpus = [
            placed(1, region(900, 1_900, 1_100, 2_100)),
            placed(2, at(1_000, 2_000)),
        ];
        let measured = refinement(&corpus).expect("a region and a point measure");
        assert_eq!(measured.admitted, 2, "each meets the other");
        assert_eq!(measured.reached, 2);
        // Reached equals admitted, so the covering wasted nothing: a ratio of
        // exactly one hundred per cent, calculable by hand from two records.
        assert_eq!(measured.refinement(), Some(100));
    }
}
