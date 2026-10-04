use super::{Aggregate, Retention};

/// The whole membership of [`Retention::WholeGroup`], asserted as a set.
///
/// The same guard `Purity`'s membership tests give, for the same reason and
/// against a sharper failure. `retention` forces a new fold to be
/// *classified*, but the classification is a claim about what the fold costs
/// and the two can be written apart. Both directions are wrong and only one
/// is loud: a constant-space fold listed here is merely refused a memory
/// exemption it deserved, while a whole-group fold left out keeps the
/// exemption that says *"its answer does not grow with the table"* — and
/// then `SELECT collect(x) FROM huge` is exactly the unbounded read the
/// ceiling exists to refuse, waved through by name (Q-227).
///
/// Adding a member is therefore allowed and cheap; the test exists so that
/// **failing to** add one cannot happen quietly.
#[test]
fn the_folds_that_hold_their_whole_group_are_exactly_the_ones_that_must() {
    let holding: Vec<&str> = Aggregate::ALL
        .iter()
        .filter(|fold| fold.retention() == Retention::WholeGroup)
        .map(|fold| fold.spelling())
        .collect();
    // The counter folds order their samples by instant only once the group
    // has arrived, so they hold it (ADR-0088 §5).
    assert_eq!(holding, ["median", "increase", "rate", "delta", "collect"]);
}

/// Every fold is in `ALL`, and every spelling parses back to itself.
///
/// `ALL` is what the parser reads to recognise a fold at all, so a variant
/// missing from it is a fold nobody can write — and no other test would
/// notice, because the grammar simply treats the word as a field name.
#[test]
fn every_fold_is_listed_and_every_spelling_names_it_back() {
    for fold in Aggregate::ALL {
        assert_eq!(
            Aggregate::parse(fold.spelling()),
            Some(*fold),
            "{} did not parse back to itself",
            fold.spelling()
        );
    }
    let spellings: Vec<&str> = Aggregate::ALL.iter().map(|fold| fold.spelling()).collect();
    let mut sorted = spellings.clone();
    sorted.sort_unstable();
    sorted.dedup();
    assert_eq!(
        sorted.len(),
        spellings.len(),
        "two folds share a spelling: {spellings:?}"
    );
}
