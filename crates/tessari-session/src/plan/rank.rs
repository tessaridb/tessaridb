use core::cmp::Ordering;

use super::candidate::Candidate;

/// The candidate that promises to narrow the most.
///
/// `None` when nothing can be served by an index, which is the scan.
///
/// Total and pure: it reads no store and can therefore be tested over the whole
/// ranking matrix directly, rather than inferred from how long a query took.
pub(crate) fn choose(candidates: Vec<Candidate>) -> Option<Candidate> {
    // A later candidate must be strictly better to displace an earlier one, so
    // an equal one loses and source order survives — which is what makes the
    // plan predictable from the condition the author wrote.
    candidates
        .into_iter()
        .reduce(|best, next| if better(&next, &best) { next } else { best })
}

/// Whether the first candidate promises fewer records than the second.
///
/// The order of the three tests is the whole rule, and the middle one moved
/// here. A ceiling decides first because it is a counted fact. Then the number
/// of fields narrowed, because **that is a proof** — a subset cannot be larger
/// than the set it is drawn from — and a proof outranks the shape ranking, which
/// its own doc describes as what a candidate is *trusted* to narrow when nothing
/// exact is known. Shape decides last, among candidates that narrow equally
/// many fields, and an equal shape still falls through to source order.
///
/// Placing the count below the shape, as it was, made a composite range
/// unreachable: `a = 1 AND b > 2` on `(a, b)` offers an equality fixing one
/// field and a range fixing one and bounding a second, and `Shape::Equality`
/// sorts before `Shape::Range`, so the narrower candidate lost to the wider one
/// on a heuristic. No decision this store made before changes, because every
/// candidate that narrowed more than one field was an equality, which the shape
/// ranking already preferred.
fn better(candidate: &Candidate, than: &Candidate) -> bool {
    match candidate.rows.rank(than.rows) {
        Ordering::Less => true,
        Ordering::Greater => false,
        Ordering::Equal => match candidate.served.fixed().cmp(&than.served.fixed()) {
            Ordering::Greater => true,
            Ordering::Less => false,
            Ordering::Equal => candidate.served.shape() < than.served.shape(),
        },
    }
}

#[cfg(test)]
mod tests {
    use tessari_geo::{Bounds, Cell, Relation, Snapped};
    use tessari_storage::IndexDefinition;
    use tessari_types::{DatabaseId, IndexId, NamespaceId, Path, TableId, Value};

    use super::choose;
    use crate::plan::candidate::{Candidate, Rows, Served, Shape};

    fn index(name: &str, unique: bool, search: bool) -> IndexDefinition {
        IndexDefinition {
            id: IndexId::new(1),
            namespace: NamespaceId::new(1),
            database: DatabaseId::new(1),
            table: TableId::new(1),
            name: name.to_owned(),
            fields: vec![Path::field("x")],
            search,
            unique,
            vector: None,
            spatial: false,
        }
    }

    fn candidate(name: &str, shape: Shape, rows: Rows) -> Candidate {
        let served = match shape {
            Shape::Equality => Served::Equality(vec![Value::from("x")]),
            Shape::Prefix => Served::Prefix("x".to_owned()),
            Shape::Terms => Served::Terms(vec!["x".to_owned()]),
            Shape::Range => Served::Range {
                fixed: Vec::new(),
                lower: Some(Value::from("a")),
                upper: Some(Value::from("z")),
            },
            Shape::Region => Served::Region {
                cells: vec![Cell::root()],
                bounds: Bounds::of_position(
                    Snapped::from_units(0, 0).expect("the origin is on the grid"),
                ),
                relation: Relation::Meets,
            },
        };
        Candidate {
            served,
            index: index(
                name,
                shape == Shape::Equality && rows != Rows::Unknown,
                shape == Shape::Terms,
            ),
            rows,
        }
    }

    fn winner(candidates: Vec<Candidate>) -> String {
        choose(candidates).expect("a candidate").index.name
    }

    #[test]
    fn nothing_to_serve_is_the_scan() {
        assert!(choose(Vec::new()).is_none());
    }

    #[test]
    fn a_known_ceiling_beats_an_unknown_one_whichever_was_written_first() {
        // The case the whole module exists for: `email` is unique and selects
        // one record, `city` is not and was written first.
        assert_eq!(
            winner(vec![
                candidate("by_city", Shape::Equality, Rows::Unknown),
                candidate("by_email", Shape::Equality, Rows::AtMost(1)),
            ]),
            "by_email"
        );
        assert_eq!(
            winner(vec![
                candidate("by_email", Shape::Equality, Rows::AtMost(1)),
                candidate("by_city", Shape::Equality, Rows::Unknown),
            ]),
            "by_email"
        );
    }

    #[test]
    fn the_smaller_of_two_known_ceilings_wins() {
        assert_eq!(
            winner(vec![
                candidate("wide", Shape::Terms, Rows::AtMost(900)),
                candidate("narrow", Shape::Terms, Rows::AtMost(3)),
            ]),
            "narrow"
        );
    }

    #[test]
    fn an_ordered_range_ranks_with_a_prefix_and_below_a_value() {
        // Both are ranges that can be the whole table, and neither's size is
        // knowable without doing the read.
        assert_eq!(
            winner(vec![
                candidate("by_range", Shape::Range, Rows::Unknown),
                candidate("by_value", Shape::Equality, Rows::Unknown),
            ]),
            "by_value"
        );
        assert_eq!(
            winner(vec![
                candidate("by_prefix", Shape::Prefix, Rows::Unknown),
                candidate("by_range", Shape::Range, Rows::Unknown),
            ]),
            "by_prefix",
            "ties keep the one written first"
        );
    }

    #[test]
    fn a_value_beats_a_range_when_neither_is_known() {
        // A prefix range can be most of the table — `LIKE 'a%'` — where an
        // equality is bounded by the records holding one value.
        assert_eq!(
            winner(vec![
                candidate("by_prefix", Shape::Prefix, Rows::Unknown),
                candidate("by_value", Shape::Equality, Rows::Unknown),
            ]),
            "by_value"
        );
        assert_eq!(
            winner(vec![
                candidate("by_value", Shape::Equality, Rows::Unknown),
                candidate("by_prefix", Shape::Prefix, Rows::Unknown),
            ]),
            "by_value"
        );
    }

    #[test]
    fn a_known_ceiling_beats_a_range_however_large_the_ceiling_is() {
        // Deliberately: an unknown is unknown, and a term held by nine hundred
        // documents is still a promise where `LIKE 'a%'` is not.
        assert_eq!(
            winner(vec![
                candidate("by_prefix", Shape::Prefix, Rows::Unknown),
                candidate("by_terms", Shape::Terms, Rows::AtMost(900)),
            ]),
            "by_terms"
        );
    }

    #[test]
    fn two_equal_candidates_keep_the_one_written_first() {
        // So that two runs of one statement cannot disagree, and an author can
        // predict the plan from the condition they wrote.
        assert_eq!(
            winner(vec![
                candidate("first", Shape::Equality, Rows::AtMost(1)),
                candidate("second", Shape::Equality, Rows::AtMost(1)),
            ]),
            "first"
        );
        assert_eq!(
            winner(vec![
                candidate("first", Shape::Prefix, Rows::Unknown),
                candidate("second", Shape::Prefix, Rows::Unknown),
            ]),
            "first"
        );
    }

    #[test]
    fn a_ceiling_of_zero_is_the_best_candidate_there_is() {
        // A term nothing holds: the read is empty, and no other candidate can
        // beat producing nothing.
        assert_eq!(
            winner(vec![
                candidate("by_email", Shape::Equality, Rows::AtMost(1)),
                candidate("by_terms", Shape::Terms, Rows::AtMost(0)),
            ]),
            "by_terms"
        );
    }
}
