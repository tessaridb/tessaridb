use super::*;

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
