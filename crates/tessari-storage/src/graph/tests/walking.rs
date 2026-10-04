use super::*;

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
            node.neighbours.len() <= super::super::NEIGHBOURS,
            "{id} kept {}",
            node.neighbours.len()
        );
        assert!(!node.neighbours.contains(id), "{id} points at itself");
    }
}
