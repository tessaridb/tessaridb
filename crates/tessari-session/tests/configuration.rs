//! `DEFINE NODE`, `DEFINE REPLICA`, `INFO FOR NODE` — the node configured in
//! the language rather than in a file beside it.
//!
//! The claim these hold is not "the statements work". It is that the **line**
//! between the two halves is where ADR-0018 draws it, and that the answer shows
//! the line rather than hiding it.
//!
//! - What describes **this machine** — its roles, its address — goes to the
//!   local `META` keyspace, which the log does not carry.
//! - What describes the **topology** — which peers exist — is a catalog record,
//!   which the log does carry and every node therefore learns.
//!
//! `INFO FOR NODE` answers both as two named groups, because a reader has to be
//! able to tell which fields would follow a backup. That the peer list is
//! *under* `cluster` rather than beside `roles` is asserted here as a property
//! and not as formatting: it is the only thing in the answer that says which
//! half a field belongs to, and the bad day it is guessed wrong on is the one
//! where a restore produces a second claimant to one identity.
//!
//! The half these tests do **not** hold is the one that needs a backup to show:
//! that the peer list actually survives a restore while the identity does not.
//! That is the pair, and it lives in `tessari-backup/tests/restore.rs` against a
//! fixture that exercises every engine — neither half of it is the criterion on
//! its own.

#![allow(clippy::panic, clippy::unwrap_used, clippy::indexing_slicing)]

use std::sync::Arc;

use tessari_kv::{KvBackend, MemoryBackend};
use tessari_session::{Outcome, Session};
use tessari_storage::Store;
use tessari_types::Value;

const PASSWORD: &str = "correct horse battery";

fn backend() -> Arc<dyn KvBackend> {
    Arc::new(MemoryBackend::new())
}

/// A store closed by an owner, so that permission is actually being tested.
fn closed(backend: &Arc<dyn KvBackend>) -> Store {
    let store = Store::open(Arc::clone(backend)).unwrap();
    {
        let mut session = Session::new(&store);
        session
            .run(&format!(
                "DEFINE USER root ROLE owner PASSWORD '{PASSWORD}';"
            ))
            .unwrap();
    }
    store
}

fn owner(store: &Store) -> Session<'_> {
    let mut session = Session::new(store);
    session.sign_in("root", PASSWORD).unwrap();
    session
}

/// What `INFO FOR NODE` reports, as an owner.
fn reported(store: &Store) -> std::collections::BTreeMap<String, Value> {
    let outcomes = owner(store).run("INFO FOR NODE;").unwrap();
    let Some(Outcome::Value(Value::Object(fields))) = outcomes.first() else {
        panic!("not a report: {outcomes:?}");
    };
    fields.clone()
}

/// The peers a report names, as `(name, endpoint)` pairs.
fn peers(report: &std::collections::BTreeMap<String, Value>) -> Vec<(String, String)> {
    let Some(Value::Object(cluster)) = report.get("cluster") else {
        panic!("no cluster group: {report:?}");
    };
    let Some(Value::Array(found)) = cluster.get("peers") else {
        panic!("no peer list: {cluster:?}");
    };
    found
        .iter()
        .map(|peer| {
            let Value::Object(fields) = peer else {
                panic!("not a peer: {peer:?}");
            };
            let text = |key: &str| match fields.get(key) {
                Some(Value::String(value)) => value.clone(),
                other => panic!("{key} is {other:?}"),
            };
            (text("name"), text("endpoint"))
        })
        .collect()
}

#[test]
fn a_fresh_node_reports_both_halves_and_an_empty_topology() {
    // The shape before anything is configured, because that is what every later
    // assertion is a change *from*. An empty peer list is an answer here, not an
    // absence: a node standing alone has a topology and it has one member.
    let report = reported(&closed(&backend()));

    for named in [
        "id",
        "roles",
        "membership",
        "version",
        "endpoints",
        "cluster",
    ] {
        assert!(report.contains_key(named), "no {named}: {report:?}");
    }
    assert_eq!(report.get("membership"), Some(&Value::from("alone")));
    assert_eq!(report.get("endpoints"), Some(&Value::Array(Vec::new())));
    assert!(peers(&report).is_empty(), "{report:?}");
}

#[test]
fn the_local_half_and_the_replicated_half_are_named_apart() {
    // The grouping is the point of the statement, and this is the assertion that
    // holds it. A flat object carrying the same values would be the defect
    // ADR-0020 §3 refuses: the reader could no longer tell which fields would
    // follow a backup, and remembering that is what fails on the bad day.
    let store = closed(&backend());
    owner(&store)
        .run("DEFINE NODE ROLES serving ENDPOINTS 'here:9000'; DEFINE REPLICA second AT 'there:9001';")
        .unwrap();
    let report = reported(&store);

    // Local, flat.
    assert_eq!(
        report.get("endpoints"),
        Some(&Value::Array(vec![Value::from("here:9000")]))
    );
    // Replicated, nested — and *not* also present at the top level, which is the
    // half of "two groups" that a merged answer would still satisfy.
    assert_eq!(
        peers(&report),
        vec![("second".to_owned(), "there:9001".to_owned())]
    );
    assert!(!report.contains_key("peers"), "{report:?}");
}

#[test]
fn what_the_statement_sets_survives_a_restart() {
    // Configuration that lasted only as long as the process would be a runtime
    // flag wearing a statement's clothes. Re-**opened** rather than re-read, for
    // the reason wave 58's restore test had to be: the question is what is on
    // disk, and a handle can answer from something it read earlier.
    let held = backend();
    let store = closed(&held);
    owner(&store)
        .run("DEFINE NODE ROLES serving, coordinating ENDPOINTS 'here:9000', 'here:9443';")
        .unwrap();
    drop(store);

    let reopened = Store::open(Arc::clone(&held)).unwrap();
    let report = reported(&reopened);
    assert_eq!(
        report.get("roles"),
        Some(&Value::Array(vec![
            Value::from("serving"),
            Value::from("coordinating"),
        ])),
        "{report:?}"
    );
    assert_eq!(
        report.get("endpoints"),
        Some(&Value::Array(vec![
            Value::from("here:9000"),
            Value::from("here:9443"),
        ])),
        "{report:?}"
    );
}

#[test]
fn a_clause_left_out_leaves_its_field_alone() {
    // The alternative — an absent clause clearing its field — makes
    // `DEFINE NODE ENDPOINTS …` a silent way to strip a node of its roles, and
    // an operator would find out from the routing rather than from the
    // statement.
    let store = closed(&backend());
    let mut session = owner(&store);
    session
        .run("DEFINE NODE ROLES coordinating ENDPOINTS 'here:9000';")
        .unwrap();
    session.run("DEFINE NODE ENDPOINTS 'moved:9000';").unwrap();
    let report = reported(&store);

    assert_eq!(
        report.get("roles"),
        Some(&Value::Array(vec![Value::from("coordinating")])),
        "the endpoints clause cleared the roles: {report:?}"
    );
    assert_eq!(
        report.get("endpoints"),
        Some(&Value::Array(vec![Value::from("moved:9000")]))
    );
}

#[test]
fn what_a_clause_names_replaces_what_was_there() {
    // A list is the whole story, which is the rule a grant's field list already
    // follows. Written as its own test because the previous one proves the
    // opposite property, and a reader who saw only that one could reasonably
    // conclude that clauses accumulate.
    let store = closed(&backend());
    let mut session = owner(&store);
    session
        .run("DEFINE NODE ROLES serving, writable, coordinating;")
        .unwrap();
    session.run("DEFINE NODE ROLES serving;").unwrap();

    assert_eq!(
        reported(&store).get("roles"),
        Some(&Value::Array(vec![Value::from("serving")]))
    );
}

#[test]
fn a_role_the_store_does_not_know_is_refused_and_named() {
    // Refused by the store rather than by the grammar, for the reason a vector
    // distance is: which roles exist is the store's question, and this is where
    // it knows what it knows. The refusal names the word so an operator does not
    // have to guess which of three they misspelled.
    let store = closed(&backend());
    let refused = owner(&store)
        .run("DEFINE NODE ROLES serving, leader;")
        .unwrap_err()
        .to_string();
    assert!(refused.contains("leader"), "{refused}");
}

#[test]
fn a_statement_that_sets_nothing_is_refused_rather_than_accepted_as_a_no_op() {
    // A half-written statement that succeeds is how an operator comes to believe
    // a node was configured.
    assert!(owner(&closed(&backend())).run("DEFINE NODE;").is_err());
}

#[test]
fn a_peer_declared_twice_is_refused_unless_the_statement_says_otherwise() {
    // The name is what makes a peer one peer. Without this, a re-run of a
    // provisioning script would double the topology.
    let store = closed(&backend());
    let mut session = owner(&store);
    session
        .run("DEFINE REPLICA second AT 'there:9001';")
        .unwrap();
    assert!(
        session
            .run("DEFINE REPLICA second AT 'there:9001';")
            .is_err()
    );
    session
        .run("DEFINE REPLICA IF NOT EXISTS second AT 'elsewhere:9001';")
        .unwrap();

    // And the accepted re-run left the first declaration standing rather than
    // quietly moving the address — `IF NOT EXISTS` is not an upsert.
    assert_eq!(
        peers(&reported(&store)),
        vec![("second".to_owned(), "there:9001".to_owned())]
    );
}

#[test]
fn peers_are_reported_in_name_order_whatever_order_they_were_declared_in() {
    // Two nodes comparing peer lists is the entire reason to have one, and a
    // list whose order depends on the order somebody wrote a script in cannot be
    // compared.
    let store = closed(&backend());
    owner(&store)
        .run(
            "DEFINE REPLICA charlie AT 'c:9001'; \
             DEFINE REPLICA alpha AT 'a:9001'; \
             DEFINE REPLICA bravo AT 'b:9001';",
        )
        .unwrap();

    let named: Vec<String> = peers(&reported(&store))
        .into_iter()
        .map(|(name, _)| name)
        .collect();
    assert_eq!(named, vec!["alpha", "bravo", "charlie"]);
}

#[test]
fn a_viewer_is_refused_all_three_rather_than_passed_over_by_an_empty_grant_check() {
    // The vacuity guard, and it is the same one `$node` needed: none of these
    // statements names a table, so a rule shaped "every table it names is
    // granted" is true of them for a reason that has nothing to do with
    // permission. A viewer reading `INFO FOR NODE` would otherwise be handed the
    // address of every machine holding this store's data.
    let store = closed(&backend());
    owner(&store)
        .run(&format!(
            "DEFINE USER ada ROLE viewer PASSWORD '{PASSWORD}';"
        ))
        .unwrap();

    let mut viewer = Session::new(&store);
    viewer.sign_in("ada", PASSWORD).unwrap();
    for statement in [
        "INFO FOR NODE;",
        "DEFINE NODE ROLES serving;",
        "DEFINE REPLICA second AT 'there:9001';",
    ] {
        let refused = viewer.run(statement).unwrap_err().to_string();
        // Refused for what the statement *needs*, not for a table it failed to
        // name — which is the difference between a rule and an emptiness.
        assert!(refused.contains("administer"), "{statement}: {refused}");
    }
}

#[test]
fn an_editor_may_shape_data_and_still_not_configure_the_node() {
    // The line `Needs::of` draws, tested where it actually bites: the other
    // `DEFINE`s are `Write` because an editor is expected to shape the data they
    // own, and these two are not that.
    let store = closed(&backend());
    owner(&store)
        .run(&format!("DEFINE USER e ROLE editor PASSWORD '{PASSWORD}';"))
        .unwrap();

    let mut editor = Session::new(&store);
    editor.sign_in("e", PASSWORD).unwrap();
    editor.run("DEFINE NAMESPACE prod;").unwrap();
    assert!(editor.run("DEFINE NODE ROLES serving;").is_err());
    assert!(
        editor
            .run("DEFINE REPLICA second AT 'there:9001';")
            .is_err()
    );
}

#[test]
fn a_table_called_node_is_still_an_ordinary_table() {
    // `NODE` is a contextual word rather than a reserved one, so the price of
    // this statement family is not a table name that data may already be using.
    // The same reasoning `INFO FOR STORE` was built on, held as a test rather
    // than as a comment in the parser.
    let store = closed(&backend());
    let mut session = owner(&store);
    session
        .run("DEFINE NAMESPACE prod; USE NAMESPACE prod; DEFINE DATABASE d; USE DATABASE d;")
        .unwrap();
    session
        .run("DEFINE TABLE node; CREATE node:1 = { name: 'a' };")
        .unwrap();

    let outcomes = session.run("SELECT * FROM node;").unwrap();
    let Some(Outcome::Records { records, .. }) = outcomes.first() else {
        panic!("not a read: {outcomes:?}");
    };
    assert_eq!(records.len(), 1);
    // And the statement family still works beside it, which is what makes the
    // two genuinely unambiguous rather than merely both accepted.
    assert!(session.run("INFO FOR NODE;").is_ok());
}
