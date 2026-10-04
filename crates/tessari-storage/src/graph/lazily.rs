//! A stored graph read node by node answers exactly as the same graph held
//! whole, and holds only what its walk reaches (G058 C2).

#![allow(clippy::unwrap_used, clippy::indexing_slicing)]

use std::sync::Arc;

use tessari_encoding::IndexAddress;
use tessari_kv::{KvBackend, MemoryBackend, WriteBatch};
use tessari_types::{DatabaseId, IndexId, NamespaceId, RecordId, TableId};

use super::{Graph, write};
use crate::catalog::VectorDistance;
use crate::store::Store;

fn address() -> IndexAddress {
    IndexAddress::new(
        NamespaceId::new(1),
        DatabaseId::new(1),
        TableId::new(1),
        IndexId::new(1),
    )
}

/// A deterministic spread of `count` vectors of `width`.
fn vectors(count: usize, width: usize) -> Vec<Vec<f64>> {
    let mut seed = 0x6c61_7a79_u64;
    (0..count)
        .map(|_| {
            (0..width)
                .map(|_| {
                    seed = seed
                        .wrapping_mul(6_364_136_223_846_793_005)
                        .wrapping_add(1_442_695_040_888_963_407);
                    f64::from(u32::try_from(seed >> 40).unwrap()) / f64::from(1_u32 << 24) - 0.5
                })
                .collect()
        })
        .collect()
}

/// Ids of three kinds, so the entry point is tested across the key grammar's
/// order rather than within one kind.
fn id(at: usize) -> RecordId {
    match at % 3 {
        0 => RecordId::Int(i64::try_from(at).unwrap().saturating_sub(500)),
        1 => RecordId::from(format!("t{at:05}").as_str()),
        _ => {
            let mut bytes = [0_u8; 16];
            bytes[15] = u8::try_from(at % 251).unwrap();
            bytes[14] = u8::try_from(at / 251 % 251).unwrap();
            RecordId::Uuid(bytes)
        }
    }
}

/// The same graph built whole in memory and written to a store.
fn both(count: usize, width: usize) -> (Graph, Store) {
    let mut whole = Graph::empty(VectorDistance::Cosine, false);
    let mut written = std::collections::BTreeMap::new();
    for (at, vector) in vectors(count, width).into_iter().enumerate() {
        written.extend(whole.insert(&id(at), vector).unwrap());
    }
    let store = Store::open(Arc::new(MemoryBackend::new()) as Arc<dyn KvBackend>).unwrap();
    let batch = write(WriteBatch::new(), &address(), &written);
    store.backend().apply(batch).unwrap();
    (whole, store)
}

#[test]
fn a_graph_read_node_by_node_answers_as_the_whole_graph_does() {
    let (whole, store) = both(600, 8);
    let stored = Graph::read(&store, &address(), VectorDistance::Cosine, false).unwrap();
    assert_eq!(
        stored.entry().unwrap(),
        whole.entry().unwrap(),
        "the entry point"
    );
    for query in vectors(40, 8) {
        assert_eq!(
            stored.nearest(&query, 10, None).unwrap(),
            whole.nearest(&query, 10, None).unwrap()
        );
    }
}

#[test]
fn a_walk_holds_only_the_nodes_it_reaches() {
    let (_, store) = both(2_000, 8);
    let stored = Graph::read(&store, &address(), VectorDistance::Cosine, false).unwrap();
    let found = stored.nearest(&vectors(1, 8)[0], 10, None).unwrap();
    assert_eq!(found.len(), 10);
    let held = stored.nodes.held();
    assert!(
        held > 0 && held < 1_000,
        "a walk over 2 000 nodes held {held}"
    );
}

#[test]
fn edits_in_one_operation_are_seen_by_its_later_walks_and_the_entry() {
    // A write batch removes the smallest id and adds a smaller one: the entry
    // moves exactly as it would in the graph held whole.
    let (mut whole, store) = both(300, 8);
    let mut stored = Graph::read(&store, &address(), VectorDistance::Cosine, false).unwrap();
    let first = whole.entry().unwrap().unwrap();
    whole.remove(&first);
    stored.remove(&first);
    assert_eq!(stored.entry().unwrap(), whole.entry().unwrap());
    let smaller = RecordId::Int(-10_000);
    let vector = vectors(1, 8)[0].clone();
    assert_eq!(
        stored.insert(&smaller, vector.clone()).unwrap(),
        whole.insert(&smaller, vector).unwrap()
    );
    assert_eq!(stored.entry().unwrap(), Some(smaller));
}
