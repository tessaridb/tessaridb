//! A split table's commits are filed, stamped and admitted by shard — G031 S2.
//!
//! Three things are asserted, and each by the only observation that proves it:
//!
//! - **where a commit landed** is read as a POSITION in a named log, never as
//!   the record being readable somewhere, because presence cannot tell a record
//!   filed in the right log from one filed in its database's;
//! - **what the log says about each mutation** is read back out of the log
//!   record itself, since that is what a follower and a filter will read;
//! - **who may commit it** is asserted by the refusal it meets, named, with the
//!   nodes it names — a test any failure satisfied would stay green while an
//!   upstream refusal quietly took its place.

#![allow(clippy::panic, clippy::unwrap_used)]

use std::sync::Arc;
use std::time::Instant;

use tessari_encoding::{NODE_ID_LEN, Roles};
use tessari_kv::{KvBackend, MemoryBackend};
use tessari_storage::{
    Catalog, Error, LEASE_TTL, Lease, Reach, RecordAddress, Store, TableKind, TableShape,
};
use tessari_types::{
    DatabaseId, Epoch, IdentityKind, NamespaceId, RecordId, Sequence, ShardId, TableId,
};

const THEM: [u8; NODE_ID_LEN] = [9; NODE_ID_LEN];
const THIRD: [u8; NODE_ID_LEN] = [8; NODE_ID_LEN];

/// A store holding `prod.shop.orders`, split at `'g'` and `'p'`, beside an
/// unsplit `prod.shop.notes`.
struct Split {
    store: Store,
    namespace: NamespaceId,
    database: DatabaseId,
    orders: TableId,
    notes: TableId,
}

impl Split {
    fn new() -> Self {
        let store = Store::open(Arc::new(MemoryBackend::new()) as Arc<dyn KvBackend>).unwrap();
        let mut transaction = store.begin().unwrap();
        let mut catalog = Catalog::new(&mut transaction);
        let namespace = catalog.create_namespace("prod").unwrap().id;
        let database = catalog.create_database(namespace, "shop").unwrap().id;
        let shape = |split: Vec<RecordId>| TableShape {
            schemafull: false,
            kind: TableKind::Table,
            identity: IdentityKind::Uuid,
            graph: None,
            conflict: None,
            split,
        };
        let orders = catalog
            .create_table(
                namespace,
                database,
                "orders",
                shape(vec![RecordId::from("g"), RecordId::from("p")]),
            )
            .unwrap()
            .id;
        let notes = catalog
            .create_table(namespace, database, "notes", shape(Vec::new()))
            .unwrap()
            .id;
        transaction.commit().unwrap();
        Self {
            store,
            namespace,
            database,
            orders,
            notes,
        }
    }

    fn shard(&self, n: u32) -> Reach {
        Reach::Shard(self.namespace, self.database, self.orders, ShardId::new(n))
    }

    fn database(&self) -> Reach {
        Reach::Database(self.namespace, self.database)
    }

    fn order(&self, id: &str) -> RecordAddress {
        RecordAddress::new(
            self.namespace,
            self.database,
            self.orders,
            RecordId::from(id),
        )
    }

    fn note(&self, id: &str) -> RecordAddress {
        RecordAddress::new(
            self.namespace,
            self.database,
            self.notes,
            RecordId::from(id),
        )
    }

    fn write(&self, addresses: &[RecordAddress]) -> Result<(), Error> {
        let mut transaction = self.store.begin()?;
        for address in addresses {
            transaction.put(address.clone(), b"{}".to_vec());
        }
        transaction.commit().map(|_| ())
    }

    /// How far this node's own log for `home` has reached.
    fn tail(&self, home: Reach) -> Sequence {
        self.store
            .committed_tail(self.store.own_log(home).unwrap())
            .unwrap()
    }

    /// Make this store one of a cluster where each range in `led` is led by the
    /// node beside it, all in one transaction (ADR-0069).
    fn led(&self, led: &[(Reach, [u8; NODE_ID_LEN])]) {
        let me = self.store.node_identity().unwrap().id;
        let mut transaction = self.store.begin().unwrap();
        let mut catalog = Catalog::new(&mut transaction);
        for (name, node, endpoint) in [
            ("them", THEM, "10.0.0.2:9081"),
            ("third", THIRD, "10.0.0.3:9081"),
        ] {
            catalog
                .create_replica(
                    name,
                    endpoint,
                    Roles::SERVING.and(Roles::WRITABLE).and(Roles::COORDINATING),
                    Some(node),
                    None,
                )
                .unwrap();
        }
        for (range, node) in led {
            let node = if *node == ME { me } else { *node };
            catalog
                .record_leadership(*range, node, Epoch::new(4))
                .unwrap();
        }
        transaction.commit().unwrap();
        self.store
            .hold(Epoch::new(4), Lease::taken_at(Instant::now(), LEASE_TTL));
    }
}

/// Stands for this store's own id in [`Split::led`], which only the store knows.
const ME: [u8; NODE_ID_LEN] = [0; NODE_ID_LEN];

// ---- S2.1 and S2.2: the stamp and the home -------------------------------

#[test]
fn a_commit_inside_one_shard_is_filed_in_that_shards_log() {
    let split = Split::new();
    let database_before = split.tail(split.database());
    split.write(&[split.order("h")]).unwrap();
    assert_eq!(split.tail(split.shard(2)), Sequence::new(1));
    assert_eq!(
        split.tail(split.database()),
        database_before,
        "the database's own log did not move"
    );
    assert_eq!(split.tail(split.shard(1)), Sequence::ZERO);
}

#[test]
fn a_commit_across_two_shards_is_filed_at_their_database() {
    let split = Split::new();
    let database_before = split.tail(split.database());
    split.write(&[split.order("a"), split.order("q")]).unwrap();
    assert_eq!(
        split.tail(split.database()).get(),
        database_before.get() + 1
    );
    assert_eq!(split.tail(split.shard(1)), Sequence::ZERO);
    assert_eq!(split.tail(split.shard(3)), Sequence::ZERO);
}

#[test]
fn the_log_record_says_which_shard_each_mutation_is_in() {
    let split = Split::new();
    split
        .write(&[split.order("a"), split.order("q"), split.note("n")])
        .unwrap();
    let log = split.store.own_log(split.database()).unwrap();
    let (_, record) = split
        .store
        .log_records(log, split.tail(split.database()), 1)
        .unwrap()
        .into_iter()
        .next()
        .unwrap();
    let shards: Vec<_> = record
        .mutations()
        .iter()
        .map(|mutation| (mutation.table, mutation.shard.map(ShardId::get)))
        .collect();
    assert_eq!(
        shards,
        vec![
            (split.orders, Some(1)),
            (split.orders, Some(3)),
            (split.notes, None),
        ]
    );
}

#[test]
fn a_boundary_identity_is_filed_in_the_shard_it_begins() {
    let split = Split::new();
    split.write(&[split.order("g")]).unwrap();
    split.write(&[split.order("p")]).unwrap();
    assert_eq!(split.tail(split.shard(2)), Sequence::new(1));
    assert_eq!(split.tail(split.shard(3)), Sequence::new(1));
}

#[test]
fn an_unsplit_table_beside_a_split_one_is_filed_as_it_always_was() {
    let split = Split::new();
    let before = split.tail(split.database());
    split.write(&[split.note("n")]).unwrap();
    assert_eq!(split.tail(split.database()).get(), before.get() + 1);
}

// ---- S2.3: leadership at a shard's grain ------------------------------

#[test]
fn a_node_writes_the_shard_it_leads_and_is_redirected_from_the_one_it_does_not() {
    let split = Split::new();
    split.led(&[(split.shard(1), ME), (split.shard(2), THEM)]);
    split.write(&[split.order("a")]).unwrap();
    let refused = split.write(&[split.order("h")]).unwrap_err();
    assert!(
        matches!(refused, Error::WriteIsElsewhere { node, ref endpoint, .. }
            if node == THEM && endpoint == "10.0.0.2:9081"),
        "{refused:?}"
    );
}

#[test]
fn a_shards_own_leadership_beats_its_databases() {
    let split = Split::new();
    split.led(&[(split.database(), THEM), (split.shard(1), ME)]);
    split.write(&[split.order("a")]).unwrap();
    let refused = split.write(&[split.order("x")]).unwrap_err();
    assert!(
        matches!(refused, Error::WriteIsElsewhere { node, .. } if node == THEM),
        "shard 3 has no row of its own and the database's covers it: {refused:?}"
    );
}

// ---- S2.4: a transaction no single node may commit --------------------

#[test]
fn a_transaction_across_shards_led_by_two_nodes_is_refused_naming_both() {
    let split = Split::new();
    split.led(&[(split.shard(1), ME), (split.shard(2), THEM)]);
    let me = split.store.node_identity().unwrap().id;
    let refused = split
        .write(&[split.order("a"), split.order("h")])
        .unwrap_err();
    let Error::SpansLeaderships { nodes } = refused else {
        panic!("expected SpansLeaderships, got {refused:?}");
    };
    let mut expected = vec![me, THEM];
    expected.sort_unstable();
    assert_eq!(nodes, expected);
}

#[test]
fn two_other_leaders_are_both_named_and_neither_is_a_redirect() {
    let split = Split::new();
    split.led(&[(split.shard(2), THEM), (split.shard(3), THIRD)]);
    let refused = split
        .write(&[split.order("h"), split.order("x")])
        .unwrap_err();
    let Error::SpansLeaderships { nodes } = refused else {
        panic!("expected SpansLeaderships, got {refused:?}");
    };
    assert_eq!(nodes, vec![THIRD, THEM]);
}

#[test]
fn two_namespaces_led_by_two_nodes_are_refused_the_same_way() {
    // Q-772 at the grain it was found at: the redirect this replaced sent the
    // client to a node that would have refused the same transaction back.
    let split = Split::new();
    let mut transaction = split.store.begin().unwrap();
    let other = Catalog::new(&mut transaction)
        .create_namespace("staging")
        .unwrap()
        .id;
    transaction.commit().unwrap();
    split.led(&[
        (Reach::Namespace(split.namespace), ME),
        (Reach::Namespace(other), THEM),
    ]);
    let elsewhere = RecordAddress::new(
        other,
        DatabaseId::new(1),
        TableId::new(1),
        RecordId::from("s"),
    );
    let refused = split.write(&[split.note("n"), elsewhere]).unwrap_err();
    assert!(
        matches!(refused, Error::SpansLeaderships { ref nodes } if nodes.len() == 2),
        "{refused:?}"
    );
}

#[test]
fn ranges_all_led_by_one_other_node_are_still_a_redirect() {
    // The control: a transaction one node CAN commit is sent there.
    let split = Split::new();
    split.led(&[(split.shard(2), THEM), (split.shard(3), THEM)]);
    let refused = split
        .write(&[split.order("h"), split.order("x")])
        .unwrap_err();
    assert!(
        matches!(refused, Error::WriteIsElsewhere { node, .. } if node == THEM),
        "{refused:?}"
    );
}
