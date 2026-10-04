#![allow(clippy::panic, clippy::unwrap_used)]

use tessari_types::{Number, RecordId, Value};

use super::{Graph, VectorDistance, separation, vector_of};

fn vector(components: &[f64]) -> Value {
    Value::Array(
        components
            .iter()
            .map(|held| Value::Number(Number::float(*held)))
            .collect(),
    )
}

fn built(points: &[(i64, [f64; 2])]) -> Graph {
    let mut graph = Graph::empty(VectorDistance::Euclidean, false);
    for (id, point) in points {
        graph.insert(&RecordId::Int(*id), point.to_vec()).unwrap();
    }
    graph
}

#[test]
fn a_vector_is_read_the_same_way_the_distances_read_one() {
    // A record the index accepts and the distance refuses would be in the
    // graph and incomparable; the reverse would be comparable and absent.
    assert_eq!(vector_of(&vector(&[1.0, 2.0])), Some(vec![1.0, 2.0]));
    assert_eq!(vector_of(&vector(&[])), None);
    assert_eq!(vector_of(&Value::from("not a vector")), None);
    assert_eq!(
        vector_of(&Value::Array(vec![Value::from("x")])),
        None,
        "an array of something that is not a number is not a vector"
    );
    assert_eq!(vector_of(&Value::None), None);
}

#[test]
fn a_squared_distance_orders_the_way_the_real_one_does() {
    // Which is the whole justification for not taking the root.
    let origin = [0.0, 0.0];
    let near = separation(VectorDistance::Euclidean, &origin, &[1.0, 0.0]);
    let far = separation(VectorDistance::Euclidean, &origin, &[3.0, 0.0]);
    assert!(near < far);
    assert!(separation(VectorDistance::Euclidean, &origin, &[1.0]).is_infinite());
    assert!(separation(VectorDistance::Euclidean, &[], &[]).is_infinite());
}

#[test]
fn an_empty_graph_answers_with_nothing_rather_than_failing() {
    let graph = Graph::empty(VectorDistance::Euclidean, false);
    assert!(graph.is_empty().unwrap());
    assert!(graph.nearest(&[1.0, 1.0], 10, None).unwrap().is_empty());
}

#[test]
fn the_walk_finds_the_nearest_on_a_line() {
    // Small enough that the exact answer is obvious by inspection, which is
    // what makes it a test of the walk rather than of a fixture.
    let graph = built(&[
        (1, [0.0, 0.0]),
        (2, [1.0, 0.0]),
        (3, [2.0, 0.0]),
        (4, [3.0, 0.0]),
        (5, [4.0, 0.0]),
    ]);
    let found = graph.nearest(&[0.1, 0.0], 2, None).unwrap();
    assert_eq!(found, vec![RecordId::Int(1), RecordId::Int(2)]);
}

#[test]
fn every_record_is_reachable_because_the_reverse_edge_is_written() {
    // Without it a new record links outward and nothing links back, so the
    // graph becomes a collection of one-way streets and a walk from the
    // entry point can never arrive.
    let graph = built(&[
        (1, [0.0, 0.0]),
        (2, [10.0, 0.0]),
        (3, [20.0, 0.0]),
        (4, [30.0, 0.0]),
    ]);
    for (id, point) in [(2_i64, [10.0, 0.0]), (3, [20.0, 0.0]), (4, [30.0, 0.0])] {
        let found = graph.nearest(&point, 1, None).unwrap();
        assert_eq!(found, vec![RecordId::Int(id)], "could not reach {id}");
    }
}

#[test]
fn the_same_records_in_the_same_order_build_the_same_graph() {
    // The property a replica depends on, and the reason there are no random
    // levels: two replicas replay one log and must agree about which records
    // are nearest.
    let points: Vec<(i64, [f64; 2])> = (0..40)
        .map(|n| {
            let held = f64::from(n);
            (i64::from(n), [held * 0.7, held * -0.3])
        })
        .collect();
    let first = built(&points);
    let second = built(&points);
    assert_eq!(first.nodes.present(), second.nodes.present());
}

#[test]
fn a_removed_record_is_no_longer_answered_with() {
    let mut graph = built(&[(1, [0.0, 0.0]), (2, [1.0, 0.0]), (3, [2.0, 0.0])]);
    graph.remove(&RecordId::Int(1));
    let found = graph.nearest(&[0.0, 0.0], 3, None).unwrap();
    assert!(!found.contains(&RecordId::Int(1)), "{found:?}");
}

#[test]
fn a_node_keeps_at_most_the_neighbours_it_is_allowed() {
    let points: Vec<(i64, [f64; 2])> = (0..60)
        .map(|n| (i64::from(n), [f64::from(n) * 0.1, 0.0]))
        .collect();
    let graph = built(&points);
    for (id, node) in &graph.nodes.present() {
        assert!(
            node.neighbours.len() <= super::NEIGHBOURS,
            "{id} kept {}",
            node.neighbours.len()
        );
        assert!(!node.neighbours.contains(id), "{id} points at itself");
    }
}

/// A point near one of forty centres, the way a real embedding sits.
///
/// The jitter is **wide and well mixed** on purpose. An earlier version took
/// it modulo sixty, which made thousands of records share a vector exactly —
/// and recall over duplicates measures which tie a sort broke, not whether a
/// search found anything. It read as a broken index for an hour.
fn clustered(n: i64, dimensions: usize) -> Vec<f64> {
    const CENTRES: i64 = 40;
    let centre = n % CENTRES;
    (0..dimensions)
        .map(|d| {
            let axis = i64::try_from(d).unwrap_or(0);
            let base = centre
                .wrapping_mul(7_919)
                .wrapping_add(axis.wrapping_mul(104_729))
                .rem_euclid(1_000);
            let mut held = n
                .wrapping_add(1)
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(axis.wrapping_mul(1_442_695_040_888_963_407));
            held ^= held >> 33;
            held = held.wrapping_mul(-49_064_778_989_728_563_i64);
            held ^= held >> 29;
            let jitter = held.rem_euclid(200).saturating_sub(100);
            let thousandths = base.saturating_add(jitter).rem_euclid(1_000);
            f64::from(i32::try_from(thousandths).unwrap_or(0)) / 1000.0
        })
        .collect()
}

#[test]
fn the_walk_finds_nearly_all_of_the_true_nearest() {
    // The one thing this index cannot promise by construction, so it is
    // **measured** here and by the benchmark harness rather than asserted in
    // prose. Two thousand clustered points in thirty-two dimensions, the
    // exact ten computed by brute force, and the overlap counted.
    //
    // The floor is deliberately below what this fixture achieves: the number
    // to defend is "the search works", not "the search scores exactly what
    // it scored the day it was written".
    const RECORDS: i64 = 2_000;
    const DIMENSIONS: usize = 32;

    let mut graph = Graph::empty(VectorDistance::Euclidean, false);
    for n in 0..RECORDS {
        graph
            .insert(&RecordId::Int(n), clustered(n, DIMENSIONS))
            .unwrap();
    }

    let mut hit = 0_usize;
    let mut asked = 0_usize;
    for q in 0..20 {
        let query = clustered(RECORDS.saturating_add(q), DIMENSIONS);
        let mut exact: Vec<(f64, i64)> = (0..RECORDS)
            .map(|n| {
                (
                    separation(VectorDistance::Euclidean, &clustered(n, DIMENSIONS), &query),
                    n,
                )
            })
            .collect();
        exact.sort_by(|left, right| left.0.total_cmp(&right.0).then(left.1.cmp(&right.1)));
        let truth: Vec<RecordId> = exact
            .iter()
            .take(10)
            .map(|(_, n)| RecordId::Int(*n))
            .collect();
        let found = graph.nearest(&query, 10, None).unwrap();
        hit = hit.saturating_add(found.iter().filter(|id| truth.contains(id)).count());
        asked = asked.saturating_add(truth.len());
    }
    let recall = hit.saturating_mul(100).checked_div(asked).unwrap_or(0);
    assert!(recall >= 90, "recall was {recall}%");
}

#[test]
fn a_quantized_graph_rescored_on_full_vectors_finds_nearly_all_of_the_true_nearest() {
    // The same fixture as the full-precision floor, built over one byte per
    // component. The walk is asked for four times the answer and the
    // candidates are re-ranked by the exact distance, as a read does.
    const RECORDS: i64 = 2_000;
    const DIMENSIONS: usize = 32;
    let mut graph = Graph::empty(VectorDistance::Euclidean, true);
    for n in 0..RECORDS {
        graph
            .insert(&RecordId::Int(n), clustered(n, DIMENSIONS))
            .unwrap();
    }
    let exact_distance = |n: i64, query: &[f64]| {
        separation(VectorDistance::Euclidean, &clustered(n, DIMENSIONS), query)
    };
    let (mut hit, mut asked) = (0_usize, 0_usize);
    for q in 0..20 {
        let query = clustered(RECORDS.saturating_add(q), DIMENSIONS);
        let mut exact: Vec<(f64, i64)> = (0..RECORDS)
            .map(|n| (exact_distance(n, &query), n))
            .collect();
        exact.sort_by(|left, right| left.0.total_cmp(&right.0).then(left.1.cmp(&right.1)));
        let truth: Vec<RecordId> = exact
            .iter()
            .take(10)
            .map(|(_, n)| RecordId::Int(*n))
            .collect();
        let mut rescored: Vec<(f64, RecordId)> = graph
            .nearest(&query, 40, None)
            .unwrap()
            .into_iter()
            .map(|id| {
                let RecordId::Int(n) = id else {
                    panic!("{id:?}")
                };
                (exact_distance(n, &query), id)
            })
            .collect();
        rescored.sort_by(|left, right| left.0.total_cmp(&right.0).then(left.1.cmp(&right.1)));
        let found: Vec<RecordId> = rescored.into_iter().take(10).map(|(_, id)| id).collect();
        hit = hit.saturating_add(found.iter().filter(|id| truth.contains(id)).count());
        asked = asked.saturating_add(truth.len());
    }
    let recall = hit.saturating_mul(100).checked_div(asked).unwrap_or(0);
    assert!(recall >= 90, "recall was {recall}%");
}

/// The recall of this graph against the exact answer over `live`.
fn recall_over(graph: &Graph, live: &[i64], dimensions: usize) -> usize {
    let mut hit = 0_usize;
    let mut asked = 0_usize;
    for q in 0..20 {
        let query = clustered(100_000_i64.saturating_add(q), dimensions);
        let mut exact: Vec<(f64, i64)> = live
            .iter()
            .map(|n| {
                (
                    separation(
                        VectorDistance::Euclidean,
                        &clustered(*n, dimensions),
                        &query,
                    ),
                    *n,
                )
            })
            .collect();
        exact.sort_by(|left, right| left.0.total_cmp(&right.0).then(left.1.cmp(&right.1)));
        let truth: Vec<RecordId> = exact
            .iter()
            .take(10)
            .map(|(_, n)| RecordId::Int(*n))
            .collect();
        let found = graph.nearest(&query, 10, None).unwrap();
        hit = hit.saturating_add(found.iter().filter(|id| truth.contains(id)).count());
        asked = asked.saturating_add(truth.len());
    }
    hit.saturating_mul(100).checked_div(asked).unwrap_or(0)
}

#[test]
fn a_smaller_budget_finds_less_and_a_larger_one_finds_more() {
    // The knob has to *do* something, and the only honest proof is a recall
    // curve: a walk that keeps four candidates in hand explores less than one
    // that keeps two hundred and fifty-six, and finds fewer of the true
    // nearest. Nothing smaller than this shows it — over a few dozen points
    // every budget finds everything, which is why the session-level tests
    // assert the plan and this one asserts the search.
    const RECORDS: i64 = 2_000;
    const DIMENSIONS: usize = 32;

    let mut graph = Graph::empty(VectorDistance::Euclidean, false);
    for n in 0..RECORDS {
        graph
            .insert(&RecordId::Int(n), clustered(n, DIMENSIONS))
            .unwrap();
    }
    let live: Vec<i64> = (0..RECORDS).collect();

    let mean = recall_over_with(&graph, &live, DIMENSIONS, Some(4));
    let generous = recall_over_with(&graph, &live, DIMENSIONS, Some(256));

    // Strictly greater, not merely different: the direction is the claim.
    // The absolute figures are not asserted, for the reason the recall test
    // above gives — the number to defend is that the dial turns the right
    // way, not what it scored the day it was written.
    assert!(
        generous > mean,
        "a budget of 256 scored {generous}% and a budget of 4 scored {mean}%"
    );
}

#[test]
fn a_budget_below_the_answer_still_fills_the_answer() {
    // `EFFORT 1 LIMIT 10` must not quietly become `LIMIT 1`. A budget that
    // overrode a bound would be a bound answering for a bound, and the caller
    // would read the short answer as "there were only that many".
    let mut graph = Graph::empty(VectorDistance::Euclidean, false);
    for n in 0..20_u32 {
        graph
            .insert(&RecordId::Int(i64::from(n)), vec![f64::from(n), 0.0])
            .unwrap();
    }
    assert_eq!(graph.nearest(&[0.0, 0.0], 10, Some(1)).unwrap().len(), 10);
}

/// The recall of this graph over `live`, at a named budget.
fn recall_over_with(
    graph: &Graph,
    live: &[i64],
    dimensions: usize,
    effort: Option<usize>,
) -> usize {
    let mut hit = 0_usize;
    let mut asked = 0_usize;
    for q in 0..20 {
        let query = clustered(100_000_i64.saturating_add(q), dimensions);
        let mut exact: Vec<(f64, i64)> = live
            .iter()
            .map(|n| {
                (
                    separation(
                        VectorDistance::Euclidean,
                        &clustered(*n, dimensions),
                        &query,
                    ),
                    *n,
                )
            })
            .collect();
        exact.sort_by(|left, right| left.0.total_cmp(&right.0).then(left.1.cmp(&right.1)));
        let truth: Vec<RecordId> = exact
            .iter()
            .take(10)
            .map(|(_, n)| RecordId::Int(*n))
            .collect();
        let found = graph.nearest(&query, 10, effort).unwrap();
        hit = hit.saturating_add(found.iter().filter(|id| truth.contains(id)).count());
        asked = asked.saturating_add(truth.len());
    }
    hit.saturating_mul(100).checked_div(asked).unwrap_or(0)
}

#[test]
fn churn_costs_recall_and_a_rebuild_gets_it_back() {
    // The failure this whole wave is about, measured on both sides of the
    // remedy rather than argued. Removing a record takes its node and the
    // edges *out* of it; the edges *into* it are left, because finding them
    // means reading every node that might point here. Nothing goes wrong
    // that anybody can see — a candidate that does not resolve produces no
    // row — and the search quietly gets worse.
    const RECORDS: i64 = 2_000;
    const DIMENSIONS: usize = 32;

    let mut graph = Graph::empty(VectorDistance::Euclidean, false);
    for n in 0..RECORDS {
        graph
            .insert(&RecordId::Int(n), clustered(n, DIMENSIONS))
            .unwrap();
    }
    // Half of them go, spread across every cluster rather than taken from
    // one end: deleting a contiguous range would remove whole regions of the
    // graph, and what is being measured is damage to the *links*, not the
    // absence of the records.
    let live: Vec<i64> = (0..RECORDS).filter(|n| n % 2 == 0).collect();
    for n in (0..RECORDS).filter(|n| n % 2 == 1) {
        graph.remove(&RecordId::Int(n));
    }

    let churned = recall_over(&graph, &live, DIMENSIONS);
    assert!(graph.dangling() > 0, "the fixture did not churn the graph");

    // The rebuild: the same live records, inserted in record-id order.
    let mut rebuilt = Graph::empty(VectorDistance::Euclidean, false);
    for n in &live {
        rebuilt
            .insert(&RecordId::Int(*n), clustered(*n, DIMENSIONS))
            .unwrap();
    }
    let after = recall_over(&rebuilt, &live, DIMENSIONS);

    assert_eq!(
        rebuilt.dangling(),
        0,
        "a rebuilt graph still points at gaps"
    );
    assert!(after >= 90, "recall after a rebuild was {after}%");
    assert!(
        after > churned,
        "the rebuild did not improve recall: {churned}% then {after}%"
    );
}

#[test]
fn an_index_with_nothing_to_measure_reports_no_figure() {
    // Absence means *never measured*, and it has to be reachable: a graph
    // with one record has no answer a walk could get wrong, so reporting a
    // triumphant 100% there would be a number describing nothing.
    assert!(
        Graph::empty(VectorDistance::Euclidean, false)
            .recall()
            .unwrap()
            .is_none()
    );

    let mut alone = Graph::empty(VectorDistance::Euclidean, false);
    alone.insert(&RecordId::Int(1), vec![1.0, 2.0]).unwrap();
    assert!(alone.recall().unwrap().is_none());
}

#[test]
fn a_measurement_does_not_count_the_query_finding_itself() {
    // The trap in measuring an index against its own vectors. Every query is
    // a stored record, so it comes back at distance zero — a free hit. Over
    // twelve points on a line the walk is exact, so the only figure that can
    // come out is 100%: if the query record were left in the answer it would
    // occupy a slot the truth does not contain, and the score would be 90%.
    // The number therefore tells the two implementations apart.
    let mut graph = Graph::empty(VectorDistance::Euclidean, false);
    for n in 0..12_u32 {
        graph
            .insert(&RecordId::Int(i64::from(n)), vec![f64::from(n), 0.0])
            .unwrap();
    }
    let measured = graph
        .recall()
        .unwrap()
        .expect("twelve records were not measured");
    assert_eq!(measured.recall, 100, "the free hit was counted");
    assert_eq!(measured.at, 10);
    assert_eq!(measured.records, 12);
    assert_eq!(measured.sample, 12, "every record should have been a query");
    assert_eq!(measured.neighbours, 16);
    assert_eq!(measured.exploration, 64);
}

#[test]
fn a_measured_recall_is_a_function_of_the_rows_and_not_of_their_order() {
    // The same property the rebuilt graph has, asserted of the figure rather
    // than of the nodes — because a measurement is written to the catalog and
    // replicated, so two replicas that received one log in different orders
    // must publish one number. The sample is taken by position in key order
    // for exactly this reason.
    const DIMENSIONS: usize = 8;

    let mut forwards = Graph::empty(VectorDistance::Euclidean, false);
    for n in 0..200_i64 {
        forwards
            .insert(&RecordId::Int(n), clustered(n, DIMENSIONS))
            .unwrap();
    }
    let mut backwards = Graph::empty(VectorDistance::Euclidean, false);
    for n in (0..200_i64).rev() {
        backwards
            .insert(&RecordId::Int(n), clustered(n, DIMENSIONS))
            .unwrap();
    }
    assert_ne!(
        forwards.nodes.present(),
        backwards.nodes.present(),
        "the fixture is not exercising order at all"
    );

    // Rebuilt the way `index::build` does it: rows in record-id order.
    let rebuild = |source: &Graph| {
        let mut held = Graph::empty(VectorDistance::Euclidean, false);
        for (id, node) in &source.nodes.present() {
            held.insert(id, node.vector.to_vec()).unwrap();
        }
        held
    };
    assert_eq!(
        rebuild(&forwards).recall().unwrap(),
        rebuild(&backwards).recall().unwrap(),
        "two replicas would publish different recalls"
    );
}

#[test]
fn a_rebuilt_graph_is_a_function_of_the_rows_and_not_of_their_order() {
    // Why a rebuild can be trusted between replicas. The incremental graph
    // is a function of log order; a rebuild inserts in record-id order,
    // which is a property of the data — so two stores that received the same
    // records in different orders rebuild to one graph.
    const DIMENSIONS: usize = 8;
    let forwards: Vec<i64> = (0..120).collect();
    let backwards: Vec<i64> = (0..120).rev().collect();

    let mut first = Graph::empty(VectorDistance::Euclidean, false);
    for n in &forwards {
        first
            .insert(&RecordId::Int(*n), clustered(*n, DIMENSIONS))
            .unwrap();
    }
    let mut second = Graph::empty(VectorDistance::Euclidean, false);
    for n in &backwards {
        second
            .insert(&RecordId::Int(*n), clustered(*n, DIMENSIONS))
            .unwrap();
    }
    assert_ne!(
        first.nodes.present(),
        second.nodes.present(),
        "the fixture is not exercising order at all"
    );

    // Both rebuilt the way `index::build` does it: rows in record-id order.
    let rebuild = |source: &Graph| {
        let mut held = Graph::empty(VectorDistance::Euclidean, false);
        for (id, node) in source.nodes.present() {
            held.insert(&id, node.vector.to_vec()).unwrap();
        }
        held
    };
    assert_eq!(
        rebuild(&first).nodes.present(),
        rebuild(&second).nodes.present()
    );
}
