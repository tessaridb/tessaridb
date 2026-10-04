use super::*;

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
