#![allow(clippy::panic, clippy::unwrap_used)]

use tessari_types::{Number, RecordId, Value};

use super::{Graph, VectorDistance, separation, vector_of};

mod measuring;
mod recall;
mod walking;

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
