//! A write that waits for copies (G053 SG2, ADR-0106), against one store.
//!
//! The followers are not real here: what a voter holds is what the store has
//! recorded it holding, which is what a follower's ask records on a live node.
//! That keeps every case to its own seam — the level a write waits for, whether
//! the voters can meet it, and what the caller is told — while the multi-process
//! kill test proves the whole path.

use std::sync::Arc;
use std::time::{Duration, Instant};

use tessari_kv::MemoryBackend;
use tessari_session::{Error, Outcome, Session};
use tessari_storage::{Catalog, Lease, Reach, Store};
use tessari_types::{Epoch, Sequence};

/// Two peer ids, neither of them this store's.
const FIRST: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const SECOND: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

fn node(hex: &str) -> [u8; tessari_storage::NODE_ID_LEN] {
    let mut id = [0_u8; tessari_storage::NODE_ID_LEN];
    for (pair, slot) in hex.as_bytes().chunks(2).zip(id.iter_mut()) {
        *slot = u8::from_str_radix(std::str::from_utf8(pair).unwrap(), 16).unwrap();
    }
    id
}

/// A store whose namespace `prod` says `acknowledge`, declaring two voters
/// that replicate `replicates`, and holding a leadership so it may write.
fn clustered(acknowledge: &str, replicates: &str) -> Store {
    let store = Store::open(Arc::new(MemoryBackend::new())).unwrap();
    let mut session = Session::new(&store);
    session
        .run(&format!(
            "DEFINE NAMESPACE prod REPLICATION FACTOR 3 {acknowledge}; \
             DEFINE NAMESPACE other REPLICATION FACTOR 3; \
             USE NAMESPACE prod; DEFINE DATABASE app; USE DATABASE app; \
             DEFINE COLLECTION t; \
             BEGIN; \
             DEFINE REPLICA first AT 'a:9001' NODE '{FIRST}' ROLES coordinating \
                 REPLICATES NAMESPACE {replicates}; \
             DEFINE REPLICA second AT 'b:9001' NODE '{SECOND}' ROLES coordinating \
                 REPLICATES NAMESPACE {replicates}; \
             COMMIT;"
        ))
        .unwrap();
    store.hold(Epoch::new(1), Lease::taken(Duration::from_secs(60)));
    store
}

fn writing(store: &Store) -> Session<'_> {
    let mut session = Session::new(store);
    session
        .run("USE NAMESPACE prod; USE DATABASE app;")
        .unwrap();
    session
}

fn held(session: &mut Session<'_>) -> usize {
    let outcomes = session.run("SELECT * FROM t;").unwrap();
    match outcomes.last() {
        Some(Outcome::Records { records, .. }) => records.len(),
        other => panic!("not records: {other:?}"),
    }
}

/// Record that `peer` was sent, and holds, everything of `prod.app`'s log.
fn acknowledged_by(store: &Store, peer: &str) {
    let mut transaction = store.begin().unwrap();
    let catalog = Catalog::new(&mut transaction);
    let namespace = catalog.namespace_id("prod").unwrap().unwrap();
    let database = catalog.database_id(namespace, "app").unwrap().unwrap();
    let log = store
        .line_log(Reach::Database(namespace, database))
        .unwrap();
    store.follower_sent(node(peer), log, Sequence::new(1_000_000));
    store.follower_asked(node(peer), log, Sequence::new(1_000_000));
}

#[test]
fn a_single_node_is_its_own_majority_and_waits_for_nobody() {
    let store = Store::open(Arc::new(MemoryBackend::new())).unwrap();
    let mut session = Session::new(&store);
    session
        .run(
            "DEFINE NAMESPACE prod REPLICATION FACTOR 3 ACKNOWLEDGE MAJORITY; \
             USE NAMESPACE prod; DEFINE DATABASE app; USE DATABASE app; \
             DEFINE COLLECTION t;",
        )
        .unwrap();
    let began = Instant::now();
    session.run("CREATE t:1 = { a: 1 };").unwrap();
    assert!(
        began.elapsed() < Duration::from_millis(150),
        "it waited for nobody"
    );
    assert_eq!(
        store.health().unwrap().acknowledgement_waits,
        0,
        "and counted none"
    );
}

#[test]
fn a_write_held_by_a_majority_is_acknowledged() {
    let store = clustered("ACKNOWLEDGE MAJORITY", "prod");
    acknowledged_by(&store, FIRST);
    let mut session = writing(&store);
    session
        .run("CREATE t:1 = { a: 1 };")
        .unwrap_or_else(|why| panic!("one voter of two besides this node is a majority: {why}"));
    // G053 C6: the wait is counted once, and it did not time out.
    let health = store.health().unwrap();
    assert_eq!(
        (
            health.acknowledgement_waits,
            health.acknowledgement_timeouts
        ),
        (1, 0)
    );
}

#[test]
fn a_write_no_voter_acknowledges_is_committed_and_said_so() {
    let store = clustered("ACKNOWLEDGE MAJORITY", "prod");
    let mut session = writing(&store);
    let refused = session.run("CREATE t:1 = { a: 1 };");
    assert!(
        matches!(
            &refused,
            Err(Error::NotAcknowledgedInTime { needed: 2, .. })
        ),
        "{refused:?}"
    );
    assert_eq!(
        held(&mut session),
        1,
        "the refusal says committed, and it is"
    );
    // G053 C6: a timeout is a wait that ran out — counted as both, with the
    // round it spent in the total.
    let health = store.health().unwrap();
    assert_eq!(
        (
            health.acknowledgement_waits,
            health.acknowledgement_timeouts
        ),
        (1, 1)
    );
    assert!(health.acknowledgement_waited > Duration::ZERO);
}

#[test]
fn a_majority_the_voters_cannot_form_is_refused_before_anything_is_written() {
    let store = clustered("ACKNOWLEDGE MAJORITY", "other");
    let mut session = writing(&store);
    let refused = session.run("CREATE t:1 = { a: 1 };");
    assert!(
        matches!(&refused, Err(Error::MajorityUnreachable { voters: 3, .. })),
        "{refused:?}"
    );
    assert_eq!(held(&mut session), 0, "a refused write was written");
}

#[test]
fn a_replicated_namespace_that_said_nothing_waits_for_a_majority() {
    let store = clustered("", "prod");
    let mut session = writing(&store);
    assert!(matches!(
        session.run("CREATE t:1 = { a: 1 };"),
        Err(Error::NotAcknowledgedInTime { .. })
    ));
    session
        .run("CREATE t:2 = { a: 2 } ACKNOWLEDGE LEADER;")
        .unwrap_or_else(|why| panic!("an unstated namespace lets a request choose: {why}"));
}

#[test]
fn a_request_below_its_namespace_is_refused_unless_the_namespace_allows_it() {
    let store = clustered("ACKNOWLEDGE MAJORITY", "prod");
    let mut session = writing(&store);
    let refused = session.run("CREATE t:1 = { a: 1 } ACKNOWLEDGE LEADER;");
    assert!(
        matches!(&refused, Err(Error::AcknowledgeBelowNamespace { .. })),
        "{refused:?}"
    );
    assert_eq!(held(&mut session), 0);

    let store = clustered("ACKNOWLEDGE MAJORITY OR WEAKER", "prod");
    let mut session = writing(&store);
    session
        .run("CREATE t:1 = { a: 1 } ACKNOWLEDGE LEADER;")
        .unwrap();
    // And inside a transaction, the level its COMMIT asks for.
    session
        .run("BEGIN; CREATE t:2 = { a: 2 }; COMMIT ACKNOWLEDGE LEADER;")
        .unwrap();
    assert_eq!(held(&mut session), 2);
}

#[test]
fn a_read_waits_for_no_copies() {
    let store = clustered("ACKNOWLEDGE MAJORITY", "prod");
    let mut session = writing(&store);
    let began = Instant::now();
    assert_eq!(held(&mut session), 0);
    assert!(
        began.elapsed() < Duration::from_millis(150),
        "a read waited for followers to hold nothing"
    );
}

/// A store in region `eu` whose namespace `prod` says `acknowledge`, beside one
/// voter in `eu` and one in `us`, both replicating `prod` (G057 C3).
fn regional(acknowledge: &str, mine: &str) -> Store {
    let store = Store::open(Arc::new(MemoryBackend::new())).unwrap();
    let me: String = store
        .node_identity()
        .unwrap()
        .id
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    let mut session = Session::new(&store);
    session
        .run(&format!(
            "DEFINE NAMESPACE prod REPLICATION FACTOR 3 {acknowledge}; \
             USE NAMESPACE prod; DEFINE DATABASE app; USE DATABASE app; \
             DEFINE COLLECTION t; \
             BEGIN; \
             DEFINE REPLICA me AT 'm:9001' NODE '{me}' ROLES coordinating \
                 REPLICATES NAMESPACE prod {mine}; \
             DEFINE REPLICA first AT 'a:9001' NODE '{FIRST}' ROLES coordinating \
                 REPLICATES NAMESPACE prod REGION 'eu'; \
             DEFINE REPLICA second AT 'b:9001' NODE '{SECOND}' ROLES coordinating \
                 REPLICATES NAMESPACE prod REGION 'us'; \
             COMMIT;"
        ))
        .unwrap();
    store.hold(Epoch::new(1), Lease::taken(Duration::from_secs(60)));
    store
}

/// G057 C3 — `LOCAL MAJORITY` waits for a majority of the voters in this
/// node's region: here itself and `first`, so `first` alone acknowledges it,
/// and `second`, in another region, does not count.
#[test]
fn a_local_majority_is_a_majority_of_the_leaders_region() {
    let store = regional("", "REGION 'eu'");
    acknowledged_by(&store, FIRST);
    let mut session = writing(&store);
    session
        .run("CREATE t:1 = { a: 1 } ACKNOWLEDGE LOCAL MAJORITY;")
        .unwrap_or_else(|why| panic!("first is eu's majority with this node: {why}"));

    let store = regional("", "REGION 'eu'");
    acknowledged_by(&store, SECOND);
    let mut session = writing(&store);
    let refused = session.run("CREATE t:1 = { a: 1 } ACKNOWLEDGE LOCAL MAJORITY;");
    assert!(
        matches!(
            &refused,
            Err(Error::NotAcknowledgedInTime { needed: 2, .. })
        ),
        "a voter in us acknowledged an eu majority: {refused:?}"
    );
    // The same copy is a majority of the whole cluster: second and this node.
    session
        .run("CREATE t:2 = { a: 2 } ACKNOWLEDGE MAJORITY;")
        .unwrap_or_else(|why| panic!("{why}"));
    // And the region is on the row `INFO FOR NODE` reports, as written.
    let outcomes = Session::new(&store).run("INFO FOR NODE;").unwrap();
    let report = format!("{outcomes:?}");
    assert!(report.contains("\"region\": String(\"us\")"), "{report}");
}

/// A node whose own row names no region has no local majority to wait for —
/// refused by name before anything is written, never read as "the whole
/// cluster" or "nobody".
#[test]
fn a_local_majority_on_a_node_with_no_region_is_refused_before_writing() {
    let store = regional("", "");
    let mut session = writing(&store);
    let refused = session.run("CREATE t:1 = { a: 1 } ACKNOWLEDGE LOCAL MAJORITY;");
    assert!(
        matches!(&refused, Err(Error::LocalMajorityWithoutRegion { .. })),
        "{refused:?}"
    );
    assert_eq!(held(&mut session), 0, "a refused write was written");
}

/// `LOCAL MAJORITY` sits between the two: weaker than `MAJORITY`, stronger
/// than `LEADER` — as a namespace default and as a request.
#[test]
fn a_local_majority_is_weaker_than_a_majority_and_stronger_than_the_leader() {
    let store = regional("ACKNOWLEDGE MAJORITY", "REGION 'eu'");
    let mut session = writing(&store);
    let refused = session.run("CREATE t:1 = { a: 1 } ACKNOWLEDGE LOCAL MAJORITY;");
    assert!(
        matches!(&refused, Err(Error::AcknowledgeBelowNamespace { .. })),
        "{refused:?}"
    );

    let store = regional("ACKNOWLEDGE LOCAL MAJORITY", "REGION 'eu'");
    acknowledged_by(&store, FIRST);
    let mut session = writing(&store);
    session
        .run("CREATE t:1 = { a: 1 };")
        .unwrap_or_else(|why| panic!("the namespace default is eu's majority: {why}"));
    let refused = session.run("CREATE t:2 = { a: 2 } ACKNOWLEDGE LEADER;");
    assert!(
        matches!(&refused, Err(Error::AcknowledgeBelowNamespace { .. })),
        "{refused:?}"
    );
    // Stronger than the default is always admitted — and it is MAJORITY's
    // wait: first and this node are a majority of three too.
    session
        .run("CREATE t:3 = { a: 3 } ACKNOWLEDGE MAJORITY;")
        .unwrap_or_else(|why| panic!("{why}"));
}

/// Q-921, the Raft rule (ADR-0112 D12): a node that has just taken a range's
/// leadership holds a tail an earlier leader wrote, which a majority may hold
/// today and a later election may still overrule. It answers that a
/// transaction's intents there are gone for good only once an entry of its
/// own leadership is held by a majority — which covers everything before it.
#[test]
fn a_new_leader_says_intents_are_gone_only_once_its_own_entry_is_held() {
    let store = clustered("", "prod");
    let mut session = writing(&store);
    // Both peers hold whatever this log will carry, so every wait below is
    // answered and only the leader rule can refuse.
    acknowledged_by(&store, FIRST);
    acknowledged_by(&store, SECOND);
    session.run("CREATE t:1 = { n: 1 };").unwrap();
    let range = {
        let mut transaction = store.begin().unwrap();
        let catalog = Catalog::new(&mut transaction);
        let namespace = catalog.namespace_id("prod").unwrap().unwrap();
        let database = catalog.database_id(namespace, "app").unwrap().unwrap();
        Reach::Database(namespace, database)
    };
    let holds = tessari_session::AcrossAsk::Holds {
        transaction: tessari_encoding::TransactionId::new(
            [7; tessari_encoding::TRANSACTION_ID_LEN],
        ),
        range,
    };
    // Re-elected at epoch 2, its tail still written at epoch 1.
    store.hold(Epoch::new(2), Lease::taken(Duration::from_secs(60)));
    let refused = session.answer_across(&holds);
    assert!(
        matches!(
            &refused,
            Err(Error::Store(
                tessari_storage::Error::LeadershipUnconfirmed { .. }
            ))
        ),
        "{refused:?}"
    );
    // Its own first entry, held by a majority: now it may answer.
    session.run("CREATE t:2 = { n: 2 };").unwrap();
    assert_eq!(
        session.answer_across(&holds).unwrap(),
        tessari_session::AcrossAnswer::Holding(false)
    );
}
