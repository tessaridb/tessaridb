//! A write that arrives at a node which may not take it.
//!
//! Two nodes over real sockets, one range, one leader (ADR-0019). The follower
//! holds no `writable` role, so a write sent to it is **routed** rather than
//! refused: it commits on the leader, and after replication both hold it.
//!
//! What makes this different from wave 69's tests: those asked the classifier
//! what a script *is*. These ask what a node *does* with one, which is the half
//! that cannot be answered without a second node on the other end of a socket.

#![allow(clippy::panic, clippy::unwrap_used, clippy::indexing_slicing)]

use std::sync::Arc;

use tessaridb::{Db, Sequence, Value};
use tessari_wire::{Answer, Client, Node};

/// A node on a loopback port the operating system picked, plus its address.
fn serving(db: &Arc<Db>) -> (Arc<Node>, String) {
    let node = Arc::new(Node::bind(Arc::clone(db), "127.0.0.1:0").unwrap());
    let address = node.address().unwrap();
    let held = Arc::clone(&node);
    drop(std::thread::spawn(move || held.serve()));
    (node, address)
}

const READY: &str = "USE NAMESPACE prod; USE DATABASE orders;";

/// Everything the leader holds that the follower does not, moved across.
///
/// This is waves 66-68's machinery, driven by hand. Nothing pulls on its own
/// yet — Q-83 — so a test that waited for the follower to catch up would wait
/// forever, and one that skipped this step would be asserting that a write
/// appeared on a node nothing ever sent it to.
fn replicate(leader: &Db, follower: &Db) -> u64 {
    // `saturating_add` rather than `+`, matching what `bootstrap` itself does:
    // the workspace denies bare arithmetic, and the next sequence after the tail
    // is exactly the shape that lint exists for.
    let from = Sequence::new(
        follower
            .store()
            .committed_tail()
            .unwrap()
            .get()
            .saturating_add(1),
    );
    let mut carried = Vec::new();
    let sent = tessari_backup::write_from(leader.store(), &mut carried, from).unwrap();
    let applied = tessari_backup::bootstrap(follower.store(), &mut carried.as_slice()).unwrap();
    assert_eq!(
        applied.records, sent.records,
        "the follower applied a different number of records than the leader sent",
    );
    sent.records
}

/// The names a `SELECT * FROM users` answers with, in the order it answers.
fn names(client: &mut Client, script: &str) -> Vec<String> {
    let answers = client.run(&format!("{READY} {script}"), None).unwrap();
    let Some(Answer::Records { records, .. }) = answers.last() else {
        panic!("not records: {answers:?}");
    };
    records
        .iter()
        .map(|(_, value)| {
            let Value::Object(fields) = value else {
                panic!("not an object: {value:?}");
            };
            let Some(Value::String(name)) = fields.get("name") else {
                panic!("no name: {fields:?}");
            };
            name.clone()
        })
        .collect()
}

/// A leader holding a table and one record, and a follower bootstrapped from it
/// which has since dropped its `writable` role.
///
/// The order is the deployment's order and not a convenience: the peer list is
/// declared **on the leader**, because it is a catalog record that replicates
/// (ADR-0009), and declaring it on the follower would put a record in the
/// follower's log that the leader's does not have — which is divergence, and
/// the next transfer would be refused for it. The follower learns the topology
/// the way it learns everything else. Only then does it drop the role, which is
/// the local half and travels nowhere (ADR-0020 §3).
fn two_nodes() -> (Arc<Db>, String, Arc<Db>, String, Arc<Node>, Arc<Node>) {
    let leader = Arc::new(Db::in_memory().unwrap());
    let (leader_node, leader_address) = serving(&leader);

    leader
        .session()
        .run(&format!(
            "DEFINE NAMESPACE prod; USE NAMESPACE prod; DEFINE DATABASE orders; \
             USE DATABASE orders; DEFINE TABLE users; \
             CREATE users:1 = {{ name: 'ada' }}; \
             DEFINE REPLICA first AT '{leader_address}' ROLES serving, writable;"
        ))
        .unwrap();

    let follower = Arc::new(Db::in_memory().unwrap());
    replicate(&leader, &follower);
    // Local half, and last: before this the node still takes writes, which is
    // what lets the statement that drops the role be run against it at all.
    follower
        .session()
        .run("DEFINE NODE ROLES serving;")
        .unwrap();
    let (follower_node, follower_address) = serving(&follower);

    (
        leader,
        leader_address,
        follower,
        follower_address,
        leader_node,
        follower_node,
    )
}

#[test]
fn a_write_sent_to_a_follower_commits_on_the_leader_and_is_visible_on_both() {
    let (leader, leader_address, follower, follower_address, _leader_node, _follower_node) =
        two_nodes();

    // The write goes to the node that may not take it.
    let mut to_follower = Client::connect(&follower_address).unwrap();
    to_follower
        .run(
            &format!("{READY} CREATE users:2 = {{ name: 'grace' }};"),
            None,
        )
        .unwrap();

    // It committed on the leader.
    let mut to_leader = Client::connect(&leader_address).unwrap();
    assert_eq!(
        names(&mut to_leader, "SELECT * FROM users;"),
        vec!["ada".to_owned(), "grace".to_owned()],
        "the forwarded write did not commit on the leader",
    );

    // And **not** on the follower, which is the half that separates a forward
    // from a write taken locally. A test that only checked the leader would
    // pass just as well against a node that wrote in both places, and writing
    // in both places is the split brain.
    assert_eq!(
        names(&mut to_follower, "SELECT * FROM users;"),
        vec!["ada".to_owned()],
        "the follower took the write itself instead of forwarding it",
    );

    // Visible on both, once what the leader committed is carried across.
    let carried = replicate(&leader, &follower);
    assert!(carried > 0, "replication carried nothing to compare");
    assert_eq!(
        names(&mut to_follower, "SELECT * FROM users;"),
        vec!["ada".to_owned(), "grace".to_owned()],
        "the follower does not hold what the leader committed",
    );
}

/// A serving node that may not write and knows of no peer that may.
///
/// It holds one record, so a read has something to answer with, and no writable
/// peer, so any forward fails **by name** rather than by timing out. That is
/// what makes the pair of tests below decisive rather than merely green: in this
/// one fixture a write demonstrably tries to leave and cannot, so a read that
/// answers cannot have tried.
fn read_only_with_no_peer() -> (Arc<Db>, String, Arc<Node>) {
    let alone = Arc::new(Db::in_memory().unwrap());
    alone
        .session()
        .run(
            "DEFINE NAMESPACE prod; USE NAMESPACE prod; DEFINE DATABASE orders; \
             USE DATABASE orders; DEFINE TABLE users; \
             CREATE users:1 = { name: 'ada' }; DEFINE NODE ROLES serving;",
        )
        .unwrap();
    let (node, address) = serving(&alone);
    (alone, address, node)
}

#[test]
fn a_node_that_knows_of_no_writable_peer_says_so() {
    // The forward's target is missing rather than unreachable, and the two have
    // different remedies — one is a `DEFINE REPLICA … ROLES writable` nobody
    // ran, the other is a peer that is down. Named separately so the operator
    // is told which.
    let (_alone, address, _node) = read_only_with_no_peer();

    let mut client = Client::connect(&address).unwrap();
    let refused = client
        .run(
            &format!("{READY} CREATE users:9 = {{ name: 'nobody' }};"),
            None,
        )
        .unwrap_err()
        .to_string();
    assert!(
        refused.contains("no peer is declared writable"),
        "did not name the missing target: {refused}",
    );
}

#[test]
fn a_read_is_answered_where_it_was_asked() {
    // Kill criterion 3 arriving by the back door: a follower that forwarded
    // reads would make every read pay for the cluster, and the single-node
    // store would pay for machinery it does not have.
    //
    // The fixture is the one above, where a write provably cannot leave. So a
    // read that answers here answered locally — there was nowhere else for it
    // to go, and had it tried it would have failed the same way the write does.
    let (_alone, address, _node) = read_only_with_no_peer();

    let mut client = Client::connect(&address).unwrap();
    assert_eq!(
        names(&mut client, "SELECT * FROM users;"),
        vec!["ada".to_owned()],
        "a read did not answer locally",
    );
}

#[test]
fn a_node_can_be_given_back_the_role_it_dropped() {
    // `DEFINE NODE` is the local half, so it runs where it was asked even on a
    // node that may not write. Were it classified as a write it would forward,
    // and then draining a follower would drain the *leader* instead — and on a
    // node with no peer, as here, there would be no spelling for reopening the
    // door at all: the statement that returns the role would be refused for
    // want of the role it returns.
    let (_alone, address, _node) = read_only_with_no_peer();
    let mut client = Client::connect(&address).unwrap();

    client
        .run("DEFINE NODE ROLES serving, writable;", None)
        .unwrap();

    client
        .run(
            &format!("{READY} CREATE users:3 = {{ name: 'hedy' }};"),
            None,
        )
        .unwrap();
    assert_eq!(
        names(&mut client, "SELECT * FROM users;"),
        vec!["ada".to_owned(), "hedy".to_owned()],
        "the node did not take a write after being given the role back",
    );
}

#[test]
fn a_forwarded_write_does_not_carry_the_session_that_sent_it() {
    // **A characterisation test: it pins what the build does, not what it
    // should do.** When Q-110 is answered this test fails, and that failure is
    // the notification.
    //
    // The forward opens a fresh session on the leader, so the `USE` this
    // connection ran earlier is not there. That contradicts a promise this
    // protocol makes in its own words — a session "lives as long as the
    // connection, because that is what a connection *is*" — and forwarding is
    // where the promise currently stops holding.
    //
    // It is not a failure of the criterion: a write whose script carries its own
    // `USE` forwards and commits, which the test above shows. It is a failure of
    // the *pattern a client would actually use*, which is to say `USE` once and
    // then write repeatedly, and it is recorded rather than papered over.
    let (_leader, _leader_address, _follower, follower_address, _leader_node, _follower_node) =
        two_nodes();
    let mut client = Client::connect(&follower_address).unwrap();

    client
        .run("USE NAMESPACE prod; USE DATABASE orders;", None)
        .unwrap();
    let refused = client
        .run("CREATE users:7 = { name: 'lise' };", None)
        .unwrap_err()
        .to_string();

    assert!(
        refused.contains("no namespace selected"),
        "expected the leader's fresh session to have no tenancy, got: {refused}",
    );

    // The same statement, with the selection travelling in the script, commits.
    // Which is what identifies the gap as the session and not the forward.
    client
        .run(
            "USE NAMESPACE prod; USE DATABASE orders; CREATE users:7 = { name: 'lise' };",
            None,
        )
        .unwrap();
}
