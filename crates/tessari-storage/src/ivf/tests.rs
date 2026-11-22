//! Training and probing, held to the determinism contract (ADR-0141 D1).

use tessari_encoding::{IndexAddress, StoreValue, VectorTrained};
use tessari_types::{DatabaseId, IndexId, NamespaceId, RecordId, TableId};

use super::training;
use crate::catalog::VectorDistance;

fn address(index: u32) -> IndexAddress {
    IndexAddress::new(
        NamespaceId::new(1),
        DatabaseId::new(1),
        TableId::new(1),
        IndexId::new(index),
    )
}

fn id(at: i64) -> RecordId {
    RecordId::Int(at)
}

/// Two clusters far apart; the centroids should land one in each and the
/// assignment should separate them.
fn clustered(count: usize) -> Vec<(RecordId, Vec<f64>)> {
    let mut rows = Vec::with_capacity(count);
    for at in 0..count {
        let base = if at % 2 == 0 { 0.0 } else { 100.0 };
        rows.push((
            id(i64::try_from(at).unwrap_or(i64::MAX)),
            vec![
                base + f64::from(u32::try_from(at % 8).unwrap_or(0)) * 0.01,
                base * 0.5,
            ],
        ));
    }
    rows
}

#[test]
fn training_is_a_pure_function_of_rows_and_identity() {
    let rows = clustered(64);
    let first = training::train(&address(7), 4, VectorDistance::Euclidean, &rows).expect("trained");
    let second =
        training::train(&address(7), 4, VectorDistance::Euclidean, &rows).expect("trained");
    assert_eq!(first, second);
    // The encoded form is what replicas compare byte for byte.
    assert_eq!(first.encode(), second.encode());
}

#[test]
fn a_different_index_identity_trains_differently() {
    let rows = clustered(64);
    let first = training::train(&address(7), 4, VectorDistance::Euclidean, &rows).expect("trained");
    let other = training::train(&address(8), 4, VectorDistance::Euclidean, &rows).expect("trained");
    assert_ne!(first.centroids, other.centroids);
}

#[test]
fn centroids_split_the_clusters_and_assignment_follows() {
    let rows = clustered(64);
    let trained =
        training::train(&address(7), 2, VectorDistance::Euclidean, &rows).expect("trained");
    assert_eq!(trained.partitions, 2);
    assert_eq!(trained.centroid(0).len(), 2);
    // The two centroids are far apart — one per cluster, not both between.
    let apart = crate::graph::separation(
        VectorDistance::Euclidean,
        trained.centroid(0),
        trained.centroid(1),
    );
    assert!(apart > 1000.0, "centroids collapsed together: {apart}");
    // A point from each cluster assigns to a different partition.
    let low = super::assign(&trained, VectorDistance::Euclidean, &[0.05, 0.0]);
    let high = super::assign(&trained, VectorDistance::Euclidean, &[100.05, 50.0]);
    assert_ne!(low, high);
}

#[test]
fn codes_name_nearest_codebook_centroids_and_round_trip() {
    let rows = clustered(64);
    let trained =
        training::train(&address(7), 2, VectorDistance::Euclidean, &rows).expect("trained");
    let coded = super::code(&trained, &[0.03, 0.0]).expect("codes");
    assert_eq!(
        coded.codes.len(),
        usize::try_from(trained.subspaces).unwrap_or(0)
    );
    // The decoded approximation is near the coded vector — the codebook does
    // approximate the subspace.
    let near = super::code_distance(
        &trained,
        VectorDistance::Euclidean,
        &coded.codes,
        &[0.03, 0.0],
    );
    assert!(near < 4.0, "code distance drifted: {near}");
    // A wrong-width vector codes to nothing.
    assert!(super::code(&trained, &[1.0, 2.0, 3.0]).is_none());
    assert!(super::code(&trained, &[]).is_none());
}

#[test]
fn probe_order_ranks_nearest_first_and_clamps() {
    let rows = clustered(64);
    let trained =
        training::train(&address(7), 2, VectorDistance::Euclidean, &rows).expect("trained");
    let probe = super::Probe {
        query: &[0.01, 0.0],
        wanted: 1,
        effort: None,
    };
    let order = super::probe_order(&trained, VectorDistance::Euclidean, probe);
    assert_eq!(order.len(), 2);
    // EFFORT clamps: one probe when the read names one, even with more asked.
    let clamped = super::probe_order(
        &trained,
        VectorDistance::Euclidean,
        super::Probe {
            effort: Some(1),
            ..probe
        },
    );
    assert_eq!(clamped.len(), 1);
}

#[test]
fn nothing_to_train_on_is_none() {
    assert!(training::train(&address(7), 2, VectorDistance::Euclidean, &[]).is_none());
    let one = vec![(id(1), vec![1.0, 2.0])];
    let trained = training::train(&address(7), 2, VectorDistance::Euclidean, &one)
        .expect("one vector still trains");
    assert_eq!(trained.partitions, 2);
    assert_eq!(trained.centroid(1).len(), 2);
}

#[test]
fn trained_state_round_trips_through_its_encoding() {
    let rows = clustered(32);
    let trained =
        training::train(&address(7), 2, VectorDistance::Euclidean, &rows).expect("trained");
    let back = VectorTrained::decode(trained.encode().as_slice()).expect("decodes");
    assert_eq!(trained, back);
}
