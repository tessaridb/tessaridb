//! A shard map that moves between a commit's placement and its write gate
//! (ADR-0095 D8).
//!
//! The split statement teaches the registry its new map under the write gate.
//! A commit that placed its records before that and takes the gate after it
//! must file them where the NEW map puts them: a record stamped into a retired
//! shard is in a log nothing will ever write again, with nothing in an error
//! state. The hook stands where the split would land, so the window is entered
//! every time rather than by chance.

use std::sync::Arc;

use tessari_kv::{KvBackend, MemoryBackend};
use tessari_types::{IdentityKind, RecordId, ShardId};

use super::AFTER_PLACEMENT;
use crate::catalog::ShardMap;
use crate::{Catalog, Reach, RecordAddress, Store, TableKind, TableShape};

#[test]
fn a_map_that_moves_before_the_gate_files_the_write_where_the_new_map_puts_it() {
    let store = Store::open(Arc::new(MemoryBackend::new()) as Arc<dyn KvBackend>)
        .expect("a memory store opens");
    let mut transaction = store.begin().expect("a transaction begins");
    let mut catalog = Catalog::new(&mut transaction);
    let namespace = catalog.create_namespace("prod").expect("namespace").id;
    let database = catalog
        .create_database(namespace, "shop")
        .expect("database")
        .id;
    let orders = catalog
        .create_table(
            namespace,
            database,
            "orders",
            TableShape {
                schemafull: false,
                kind: TableKind::Table,
                identity: IdentityKind::Uuid,
                graph: None,
                conflict: None,
                split: vec![RecordId::from("g"), RecordId::from("p")],
                partition: None,
                spread: false,
            },
        )
        .expect("a split table")
        .id;
    transaction.commit().expect("the declarations commit");
    let tail = |shard: u32| {
        store
            .committed_tail(
                store
                    .own_log(Reach::Shard(
                        namespace,
                        database,
                        orders,
                        ShardId::new(shard),
                    ))
                    .expect("a shard's log"),
            )
            .expect("its tail")
            .get()
    };
    // `'r'` is in shard 3 under `SPLIT AT 'g', 'p'` and in shard 4 under
    // `'g', 'm', 'p'`, so the log it lands in says which map filed it.
    let moved = ShardMap::declared(&[
        RecordId::from("g"),
        RecordId::from("m"),
        RecordId::from("p"),
    ])
    .expect("ordered points")
    .expect("a map");
    let (retired_before, successor_before) = (tail(3), tail(4));
    AFTER_PLACEMENT.with(|held| {
        *held.borrow_mut() = Some(Box::new(move |store: &Store| {
            store.shards().learn(orders, Some(&moved));
        }));
    });

    let mut write = store.begin().expect("a transaction begins");
    write.put(
        RecordAddress::new(namespace, database, orders, RecordId::from("r")),
        b"{}".to_vec(),
    );
    write.commit().expect("the write commits");

    assert_eq!(
        tail(3),
        retired_before,
        "the write was filed in the shard the map it was placed under put it in, after that map had moved"
    );
    assert!(
        tail(4) > successor_before,
        "the write is not in the shard the current map puts it in"
    );
}
