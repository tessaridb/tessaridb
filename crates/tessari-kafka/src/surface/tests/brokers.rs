//! Metadata on a cluster: a broker for every member a Kafka client can reach,
//! and each topic's partition led by the broker of the node leading its
//! database (G079 W5d-4).

use std::sync::Arc;

use tessari_encoding::{NODE_ID_LEN, Roles};
use tessari_storage::{Catalog, Reach, ReplicaDefinition};
use tessari_types::Epoch;
use tokio::net::TcpStream;
use tokio_util::sync::CancellationToken;

use super::admin::sign_in;
use super::ask;
use super::groups::{commit, committed, heartbeat, join_with, sync};
use crate::codec::{Reader, Writer};
use crate::surface::errors::NOT_COORDINATOR;
use crate::surface::{Database, Node};

const SETUP: &str = "DEFINE NAMESPACE app; USE NAMESPACE app; DEFINE DATABASE main; \
                     USE DATABASE main; DEFINE TOPIC orders; \
                     DEFINE NAMESPACE own; USE NAMESPACE own; DEFINE DATABASE main; \
                     USE DATABASE main; DEFINE TOPIC mine;";

const LEADER: [u8; NODE_ID_LEN] = [9; NODE_ID_LEN];
const QUIET: [u8; NODE_ID_LEN] = [3; NODE_ID_LEN];

fn row(name: &str, node: [u8; NODE_ID_LEN], kafka: Option<&str>) -> ReplicaDefinition {
    ReplicaDefinition {
        name: name.to_owned(),
        endpoint: format!("{name}:9081"),
        roles: Roles::SERVING.and(Roles::WRITABLE).and(Roles::COORDINATING),
        node: Some(node),
        replicates: Some(Reach::Store),
        leads: None,
        clients: None,
        http: None,
        kafka: kafka.map(str::to_owned),
        fingerprint: None,
        join: None,
        releasing: false,
        preferred: false,
        region: None,
    }
}

/// A node whose catalog names itself, a peer leading `app` with a Kafka
/// address, and a peer with none; this node leads `own`. Bare topic names are
/// read in `bare`. Answers the address and this node's id.
async fn a_member_of_three(bare: &str) -> (String, CancellationToken, [u8; NODE_ID_LEN]) {
    a_member_of_three_after(bare, "").await
}

/// [`a_member_of_three`], with `then` run after the setup.
async fn a_member_of_three_after(
    bare: &str,
    then: &str,
) -> (String, CancellationToken, [u8; NODE_ID_LEN]) {
    let db = Arc::new(tessaridb::Db::in_memory().await.unwrap());
    db.session().run(SETUP).await.unwrap();
    if !then.is_empty() {
        db.session().run(then).await.unwrap();
    }
    let me = db.store().node_identity().await.unwrap().id;
    let mut transaction = db.store().begin().await.unwrap();
    let mut catalog = Catalog::new(&mut transaction);
    let app = catalog.namespace_id("app").await.unwrap().unwrap();
    let own = catalog.namespace_id("own").await.unwrap().unwrap();
    for definition in [
        row("me", me, Some("10.0.0.1:9092")),
        row("leader", LEADER, Some("10.0.0.2:9092")),
        row("quiet", QUIET, None),
    ] {
        catalog.create_replica_with(definition).await.unwrap();
    }
    catalog
        .record_leadership(Reach::Namespace(app), LEADER, Epoch::new(7))
        .unwrap();
    catalog
        .record_leadership(Reach::Namespace(own), me, Epoch::new(8))
        .unwrap();
    transaction.commit().await.unwrap();
    let node = Node::bind(Arc::clone(&db), "127.0.0.1:0")
        .unwrap()
        .reading_bare_names_in(Database::parse(bare).unwrap());
    let address = node.address().unwrap();
    let stop = CancellationToken::new();
    let stopping = stop.clone();
    tokio::spawn(async move { node.serve(stopping).await });
    (address, stop, me)
}

/// What a Metadata v9 for `topics` answers: the brokers, the controller and
/// each topic's partition leader.
struct Answered {
    brokers: Vec<(i32, String, i32)>,
    controller: i32,
    leaders: Vec<(String, i32)>,
}

async fn metadata(stream: &mut TcpStream, topics: &[&str]) -> Answered {
    let mut body = Writer::new();
    body.array_len(true, Some(topics.len())).unwrap();
    for topic in topics {
        body.string(true, topic).unwrap();
        body.no_tagged_fields();
    }
    body.bool(false);
    body.bool(false);
    body.bool(false);
    body.no_tagged_fields();
    let answer = ask(stream, 3, 9, true, &body.into_bytes()).await;
    let mut reader = Reader::new(&answer);
    reader.i32("throttle").unwrap();
    let count = reader.array_len(true, "brokers").unwrap().unwrap();
    let mut brokers = Vec::new();
    for _ in 0..count {
        let id = reader.i32("node").unwrap();
        let host = reader.string(true, "host").unwrap().to_owned();
        let port = reader.i32("port").unwrap();
        reader.nullable_string(true, "rack").unwrap();
        reader.tagged_fields("broker").unwrap();
        brokers.push((id, host, port));
    }
    reader.nullable_string(true, "cluster").unwrap();
    let controller = reader.i32("controller").unwrap();
    let count = reader.array_len(true, "topics").unwrap().unwrap();
    let mut leaders = Vec::new();
    for _ in 0..count {
        assert_eq!(reader.i16("error").unwrap(), 0);
        let name = reader.string(true, "name").unwrap().to_owned();
        reader.bool("internal").unwrap();
        let partitions = reader.array_len(true, "partitions").unwrap().unwrap();
        assert_eq!(partitions, 1);
        reader.i16("error").unwrap();
        reader.i32("index").unwrap();
        let leader = reader.i32("leader").unwrap();
        reader.i32("epoch").unwrap();
        for list in ["replicas", "isr"] {
            let count = reader.array_len(true, list).unwrap().unwrap();
            for _ in 0..count {
                assert_eq!(reader.i32(list).unwrap(), leader);
            }
        }
        reader.array_len(true, "offline").unwrap();
        reader.tagged_fields("partition").unwrap();
        reader.i32("operations").unwrap();
        reader.tagged_fields("topic").unwrap();
        leaders.push((name, leader));
    }
    Answered {
        brokers,
        controller,
        leaders,
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn metadata_names_a_broker_per_member_and_the_leader_of_each_topic() {
    let (address, stop, me) = a_member_of_three("app.main").await;
    // The id of a node is its place among every member's node id, so every
    // node of the cluster numbers the brokers alike.
    let id = numbered(me);
    let mut stream = TcpStream::connect(&address).await.unwrap();
    let answered = metadata(&mut stream, &["orders", "own.main.mine"]).await;
    let mut brokers = answered.brokers;
    brokers.sort_unstable();
    let mut expected = vec![
        (id(me), "10.0.0.1".to_owned(), 9092),
        (id(LEADER), "10.0.0.2".to_owned(), 9092),
    ];
    expected.sort_unstable();
    // The quiet peer says nowhere a Kafka client reaches it, so it is no broker.
    assert_eq!(brokers, expected);
    // One controller for the whole cluster, whichever node is asked: the one
    // coordinating its groups.
    assert_eq!(answered.controller, id(LEADER));
    assert_eq!(
        answered.leaders,
        vec![
            ("orders".to_owned(), id(LEADER)),
            ("own.main.mine".to_owned(), id(me)),
        ]
    );
    stop.cancel();
}

/// The broker FindCoordinator v4 names for `group`, and its code.
async fn coordinator(stream: &mut TcpStream, group: &str) -> (i32, String, i32, i16) {
    let mut body = Writer::new();
    body.i8(0);
    body.array_len(true, Some(1)).unwrap();
    body.string(true, group).unwrap();
    body.no_tagged_fields();
    let answer = ask(stream, 10, 4, true, &body.into_bytes()).await;
    let mut reader = Reader::new(&answer);
    reader.i32("throttle").unwrap();
    reader.array_len(true, "coordinators").unwrap();
    reader.string(true, "key").unwrap();
    let node = reader.i32("node").unwrap();
    let host = reader.string(true, "host").unwrap().to_owned();
    let port = reader.i32("port").unwrap();
    (node, host, port, reader.i16("error").unwrap())
}

/// The node ids of `me` and the two peers, numbered as Metadata numbers them.
fn numbered(me: [u8; NODE_ID_LEN]) -> impl Fn([u8; NODE_ID_LEN]) -> i32 {
    let mut nodes = [me, LEADER, QUIET];
    nodes.sort_unstable();
    move |node| {
        let place = nodes.iter().position(|each| *each == node).unwrap();
        i32::try_from(place.checked_add(1).unwrap()).unwrap()
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn find_coordinator_names_the_node_leading_the_database_bare_names_are_read_in() {
    let (address, stop, me) = a_member_of_three("app.main").await;
    let id = numbered(me);
    let mut stream = TcpStream::connect(&address).await.unwrap();
    assert_eq!(
        coordinator(&mut stream, "readers").await,
        (id(LEADER), "10.0.0.2".to_owned(), 9092, 0)
    );
    stop.cancel();
    // Control: where this node leads that database, it coordinates.
    let (address, stop, me) = a_member_of_three("own.main").await;
    let id = numbered(me);
    let mut stream = TcpStream::connect(&address).await.unwrap();
    assert_eq!(
        coordinator(&mut stream, "readers").await,
        (id(me), "10.0.0.1".to_owned(), 9092, 0)
    );
    stop.cancel();
}

/// LeaveGroup v4's top-level code.
async fn leave(stream: &mut TcpStream, group: &str, member: &str) -> i16 {
    let mut body = Writer::new();
    body.string(true, group).unwrap();
    body.array_len(true, Some(1)).unwrap();
    body.string(true, member).unwrap();
    body.nullable_string(true, None).unwrap();
    body.no_tagged_fields();
    body.no_tagged_fields();
    let answer = ask(stream, 13, 4, true, &body.into_bytes()).await;
    let mut reader = Reader::new(&answer);
    reader.i32("throttle").unwrap();
    reader.i16("error").unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn a_group_request_on_a_node_that_does_not_coordinate_answers_not_coordinator() {
    let (address, stop, _) = a_member_of_three("app.main").await;
    let mut stream = TcpStream::connect(&address).await.unwrap();
    let joined = join_with(&mut stream, 7, "readers", "", b"subscription").await;
    assert_eq!(joined.code, NOT_COORDINATOR);
    assert_eq!(
        sync(&mut stream, "readers", 1, "m", &[]).await.0,
        NOT_COORDINATOR
    );
    assert_eq!(
        heartbeat(&mut stream, "readers", 1, "m").await,
        NOT_COORDINATOR
    );
    assert_eq!(leave(&mut stream, "readers", "m").await, NOT_COORDINATOR);
    assert_eq!(
        commit(&mut stream, "readers", 1, "m", "orders", 3).await,
        NOT_COORDINATOR
    );
    assert_eq!(
        committed(&mut stream, "readers", Some("orders")).await,
        vec![("orders".to_owned(), -1, NOT_COORDINATOR)]
    );
    stop.cancel();
    // Control: the node leading the database coordinates the group.
    let (address, stop, _) = a_member_of_three("own.main").await;
    let mut stream = TcpStream::connect(&address).await.unwrap();
    let joined = join_with(&mut stream, 7, "readers", "", b"subscription").await;
    assert_eq!(joined.code, 79, "MEMBER_ID_REQUIRED on a first join");
    stop.cancel();
}

#[tokio::test(flavor = "multi_thread")]
async fn a_caller_not_signed_in_to_a_closed_store_is_told_of_this_node_alone() {
    let (address, stop, me) = a_member_of_three_after(
        "app.main",
        "DEFINE USER ada ROLE editor PASSWORD 'ada secret';",
    )
    .await;
    let id = numbered(me);
    let mut stream = TcpStream::connect(&address).await.unwrap();
    // The peers' addresses are the cluster's topology, which a stranger is not
    // told; this node is where the stranger already is.
    let answered = metadata(&mut stream, &[]).await;
    assert_eq!(
        answered.brokers,
        vec![(id(me), "10.0.0.1".to_owned(), 9092)]
    );
    assert_eq!(
        coordinator(&mut stream, "readers").await,
        (id(me), "10.0.0.1".to_owned(), 9092, 0)
    );
    // Control: signed in — as a user who may not read the store's own
    // description — the same caller is told of the cluster.
    let mut signed = TcpStream::connect(&address).await.unwrap();
    sign_in(&mut signed, "ada", "ada secret").await;
    assert_eq!(metadata(&mut signed, &[]).await.brokers.len(), 2);
    assert_eq!(
        coordinator(&mut signed, "readers").await,
        (id(LEADER), "10.0.0.2".to_owned(), 9092, 0)
    );
    stop.cancel();
}
