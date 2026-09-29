//! How a measured index figure is described.

use std::collections::BTreeMap;
use tessari_encoding::{SpatialRefinement, VectorRecall};
use tessari_storage::MEASURED_RELATION;
use tessari_types::{Number, Value};

/// A measured refinement, reported with everything needed to read it.
///
/// The two ratios are given as percentages **and** the counts they came from are
/// given beside them, because the ratios answer different questions and a reader
/// who only trusts one of them should be able to recompute it. `refinement` is
/// how loose the boxes are — records offered per record kept. `fragmentation` is
/// how many entries the traversal reads per record it arrives at, which is the
/// separate failure of one record occupying many cells.
///
/// Both are `none` rather than zero when there was nothing to divide by, for the
/// reason a recall is: a zero here would read as a perfect filter.
///
/// `relation` says which query the figures answer for, and it is not decoration.
/// The measurement asks the widest relation there is, so it is the one that
/// exposes a loose covering — and a store only ever read with a narrower one
/// refines a smaller set at a cost this figure does not describe. Without the
/// label that scope is invisible: the reader sees `refinement` and has no way to
/// learn it means *refinement under `meets`*.
pub(crate) fn refining(measured: SpatialRefinement) -> Value {
    let percentage = |held: Option<u64>| {
        held.map_or(Value::None, |value| {
            Value::Number(Number::Integer(i64::try_from(value).unwrap_or(i64::MAX)))
        })
    };
    let count = |held: u64| Value::Number(Number::Integer(i64::try_from(held).unwrap_or(i64::MAX)));
    Value::Object(BTreeMap::from([
        ("relation".to_owned(), Value::from(MEASURED_RELATION.name())),
        ("refinement".to_owned(), percentage(measured.refinement())),
        (
            "fragmentation".to_owned(),
            percentage(measured.fragmentation()),
        ),
        ("entries".to_owned(), count(measured.entries)),
        ("reached".to_owned(), count(measured.reached)),
        ("admitted".to_owned(), count(measured.admitted)),
        (
            "sample".to_owned(),
            Value::Number(Number::Integer(i64::from(measured.sample))),
        ),
        ("records".to_owned(), count(measured.records)),
    ]))
}

/// A measured recall, reported with everything needed to read it.
///
/// Never the percentage alone. Recall decays as records are added after the
/// build that measured it, so a lone figure describes a store that may no longer
/// exist — `records` is what lets a reader see the store has outgrown it, and
/// `at`, `sample` and the two constants say what was actually measured.
pub(crate) fn reported(measured: VectorRecall) -> Value {
    Value::Object(BTreeMap::from([
        (
            "recall".to_owned(),
            Value::Number(Number::Integer(i64::from(measured.recall))),
        ),
        (
            "at".to_owned(),
            Value::Number(Number::Integer(i64::from(measured.at))),
        ),
        (
            "sample".to_owned(),
            Value::Number(Number::Integer(i64::from(measured.sample))),
        ),
        (
            "records".to_owned(),
            Value::Number(Number::Integer(
                i64::try_from(measured.records).unwrap_or(i64::MAX),
            )),
        ),
        (
            "neighbours".to_owned(),
            Value::Number(Number::Integer(i64::from(measured.neighbours))),
        ),
        (
            "exploration".to_owned(),
            Value::Number(Number::Integer(i64::from(measured.exploration))),
        ),
    ]))
}
