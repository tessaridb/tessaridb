//! A write sent to the wrong leader, as a client on either surface sees it
//! (G051 C2, ADR-0101).
//!
//! # The fixture
//!
//! One node leading namespace `mine` beside a declared peer leading `theirs` —
//! the arrangement `tessari-storage`'s `two_leaders` suite builds, arrived at
//! through a real `Db` so the wire door and the HTTP door serve it. The peer's
//! member row says where a CLIENT reaches it (`clients`, `http`), which is the
//! address a redirect must name: the row's `endpoint` is the peer door, and a
//! client sent there could not speak to it.
//!
//! # What each case pins
//!
//! The frame kind and the HTTP status are the routing contract, so they are
//! asserted on the surface a client reads rather than as the storage error the
//! engine has always raised — a redirect tested only in storage is how this one
//! never reached a client. Every redirect case has a control beside it: a write
//! into the namespace this node leads succeeds, so a node refusing everything
//! cannot pass.

#![allow(clippy::panic, clippy::unwrap_used, clippy::indexing_slicing)]

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::sync::Arc;
use std::time::Instant;

use tessari_encoding::{NODE_ID_LEN, Roles};
use tessari_storage::{Catalog, LEASE_TTL, Lease, Reach, ReplicaDefinition};
use tessari_types::Epoch;
use tessari_wire::{Client, Served, Settlement};
use tessaridb::{Db, Parameters};

const THEIR_NODE: [u8; NODE_ID_LEN] = [9; NODE_ID_LEN];
const PEER_DOOR: &str = "10.0.0.2:9180";
const THEIR_CLIENTS: &str = "db-2.example:9080";
const THEIR_HTTP: &str = "https://db-2.example:8000/";
const MY_EPOCH: u64 = 4;
const THEIR_EPOCH: u64 = 7;

/// A node leading `mine` beside a peer leading `theirs`, the peer's row naming
/// where clients reach it when `addressed`.
fn one_of_two(addressed: bool) -> Arc<Db> {
    let db = Db::in_memory().unwrap();
    db.session()
        .run(
            "DEFINE NAMESPACE mine; USE NAMESPACE mine; DEFINE DATABASE d; USE DATABASE d; \
             DEFINE COLLECTION c; \
             DEFINE NAMESPACE theirs; USE NAMESPACE theirs; DEFINE DATABASE d; USE DATABASE d; \
             DEFINE COLLECTION c;",
        )
        .unwrap();
    let (mine, _) = db.tenancy_in("mine", "d").unwrap().unwrap();
    let (theirs, _) = db.tenancy_in("theirs", "d").unwrap().unwrap();
    let store = db.store();
    let me = store.node_identity().unwrap().id;
    // One transaction, as a cluster is declared: the first committed member row
    // makes this node clustered, and a second commit would be judged as one.
    let mut transaction = store.begin().unwrap();
    let mut catalog = Catalog::new(&mut transaction);
    catalog
        .create_replica_with(ReplicaDefinition {
            name: "other".to_owned(),
            endpoint: PEER_DOOR.to_owned(),
            roles: Roles::SERVING.and(Roles::WRITABLE).and(Roles::COORDINATING),
            node: Some(THEIR_NODE),
            replicates: None,
            leads: None,
            clients: addressed.then(|| THEIR_CLIENTS.to_owned()),
            http: addressed.then(|| THEIR_HTTP.to_owned()),
        })
        .unwrap();
    catalog
        .record_leadership(Reach::Namespace(mine), me, Epoch::new(MY_EPOCH))
        .unwrap();
    catalog
        .record_leadership(
            Reach::Namespace(theirs),
            THEIR_NODE,
            Epoch::new(THEIR_EPOCH),
        )
        .unwrap();
    transaction.commit().unwrap();
    store.hold(
        Epoch::new(MY_EPOCH),
        Lease::taken_at(Instant::now(), LEASE_TTL),
    );
    Arc::new(db)
}

/// The wire door over `db`, and its address.
fn wire(db: &Arc<Db>) -> String {
    let node = Arc::new(tessari_wire::Node::bind(Arc::clone(db), "127.0.0.1:0").unwrap());
    let address = node.address().unwrap();
    std::thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap();
        drop(runtime.block_on(node.serve(tokio_util::sync::CancellationToken::new())));
    });
    address
}

/// The HTTP door over `db`, and its address.
fn http(db: &Arc<Db>) -> String {
    let node = Arc::new(tessari_http::Node::bind(Arc::clone(db), "127.0.0.1:0").unwrap());
    let address = node.address();
    std::thread::spawn(move || crate::serve_until_the_test_ends(&node));
    address
}

/// `POST /script`, answering the status, the `Location` header and the body.
fn post_script(address: &str, script: &str) -> (u16, Option<String>, String) {
    let mut stream = TcpStream::connect(address).unwrap();
    let head = format!(
        "POST /script HTTP/1.1\r\nHost: {address}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{script}",
        script.len()
    );
    stream.write_all(head.as_bytes()).unwrap();
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    reader.read_line(&mut line).unwrap();
    let status = line.split_whitespace().nth(1).unwrap().parse().unwrap();
    let mut location = None;
    loop {
        let mut header = String::new();
        reader.read_line(&mut header).unwrap();
        if header.trim().is_empty() {
            break;
        }
        if let Some((name, value)) = header.trim().split_once(':')
            && name.eq_ignore_ascii_case("location")
        {
            location = Some(value.trim().to_owned());
        }
    }
    let mut body = String::new();
    reader.read_to_string(&mut body).unwrap();
    (status, location, body)
}

const INTO_THEIRS: &str = "USE NAMESPACE theirs; USE DATABASE d; CREATE c:1 = { a: 1 };";
const INTO_MINE: &str = "USE NAMESPACE mine; USE DATABASE d; CREATE c:1 = { a: 1 };";

#[test]
fn a_misrouted_write_leaves_the_wire_as_a_settled_redirect_to_the_clients_address() {
    let db = one_of_two(true);
    let mut client = Client::connect(wire(&db)).unwrap();

    // The control: the namespace this node leads takes the write.
    client.run(INTO_MINE, None).unwrap();

    let served = client
        .run_routed(INTO_THEIRS, None, &Parameters::new())
        .expect("a redirect is an answer, not a failure");
    let Served::Elsewhere(sent) = served else {
        panic!("a write into a range another node leads was not redirected: {served:?}");
    };
    assert_eq!(
        sent.endpoint, THEIR_CLIENTS,
        "the redirect named the peer door, which a client cannot speak to"
    );
    assert_eq!(sent.node, THEIR_NODE);
    assert_eq!(sent.epoch, Epoch::new(THEIR_EPOCH));
    assert_eq!(
        sent.settlement,
        Settlement::Settled,
        "a leadership holds until its epoch moves, so a client may remember it"
    );
}

#[test]
fn a_peer_row_naming_no_client_address_redirects_to_the_endpoint_as_before() {
    let db = one_of_two(false);
    let mut client = Client::connect(wire(&db)).unwrap();
    let Served::Elsewhere(sent) = client
        .run_routed(INTO_THEIRS, None, &Parameters::new())
        .unwrap()
    else {
        panic!("not redirected");
    };
    assert_eq!(sent.endpoint, PEER_DOOR);
}

#[test]
fn a_transaction_refused_at_its_commit_is_redirected_whole() {
    // The write is refused at `COMMIT`, where the leadership is judged, so the
    // transaction rolled back and nothing landed: sending it again is safe.
    let db = one_of_two(true);
    let mut client = Client::connect(wire(&db)).unwrap();
    let served = client
        .run_routed(
            "USE NAMESPACE theirs; USE DATABASE d; BEGIN; CREATE c:2 = {}; CREATE c:3 = {}; COMMIT;",
            None,
            &Parameters::new(),
        )
        .unwrap();
    assert!(
        matches!(served, Served::Elsewhere(_)),
        "a whole transaction for another leader was not redirected: {served:?}"
    );
}

#[test]
fn a_script_that_already_committed_is_refused_rather_than_redirected() {
    // Following a redirect means sending the script again; this one committed
    // `mine`'s `c:9` before its second write was refused, so a redirect would
    // write it twice (ADR-0101 D3).
    let db = one_of_two(true);
    let mut client = Client::connect(wire(&db)).unwrap();
    let ran = client.run_routed(
        "USE NAMESPACE mine; USE DATABASE d; CREATE c:9 = {}; \
         USE NAMESPACE theirs; USE DATABASE d; CREATE c:9 = {};",
        None,
        &Parameters::new(),
    );
    let Err(refused) = ran else {
        panic!("a half-committed script was redirected: {ran:?}");
    };
    assert!(
        refused.to_string().contains("write it at"),
        "the refusal is not the leadership one: {refused}"
    );
    // And the first half did land, which is the reason.
    let answers = client
        .run(
            "USE NAMESPACE mine; USE DATABASE d; SELECT * FROM c:9;",
            None,
        )
        .unwrap();
    assert_eq!(answers.len(), 3);
}

#[test]
fn a_misrouted_write_over_http_is_a_307_to_the_peers_http_base() {
    let db = one_of_two(true);
    let address = http(&db);

    let (status, _, body) = post_script(&address, INTO_MINE);
    assert_eq!(status, 200, "the control write was refused: {body}");

    let (status, location, body) = post_script(&address, INTO_THEIRS);
    assert_eq!(status, 307, "{body}");
    assert_eq!(
        location.as_deref(),
        Some("https://db-2.example:8000/script"),
        "the Location is not where an HTTP client reaches the leader"
    );
}

#[test]
fn a_half_committed_script_over_http_is_a_409_and_not_a_redirect() {
    let db = one_of_two(true);
    let address = http(&db);
    let (status, location, body) = post_script(
        &address,
        "USE NAMESPACE mine; USE DATABASE d; CREATE c:7 = {}; \
         USE NAMESPACE theirs; USE DATABASE d; CREATE c:7 = {};",
    );
    assert_eq!(status, 409, "{body}");
    assert_eq!(location, None);
    assert!(body.contains("already"), "{body}");
}
