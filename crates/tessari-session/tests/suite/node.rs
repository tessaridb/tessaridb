//! `SELECT * FROM $node` — what a node says about itself.
//!
//! Three claims, and only the first is the obvious one.
//!
//! The first is that the read works through the **ordinary query path**: no new
//! route, no endpoint, nothing a console is handed privately. A `SELECT` is a
//! `SELECT`, and the only thing that is new is where its records come from
//! (ADR-0018 §3, as amended).
//!
//! The second is that the id **survives a restart**, which is what makes it an
//! identity rather than a session token — and the check that stops that from
//! passing on a constant is the one below it: two stores must get *different*
//! ids. Wave 52's lesson is exactly this shape, so it is written down as a test
//! rather than as a comment.
//!
//! The third is the permission. `$node` names no table, so the grant loop —
//! "every table this statement names is granted" — passes over it **vacuously**,
//! which is the shape that let a grant-governed owner take a whole backup. The
//! answer here is a refusal by role rather than a narrowing, because roles and
//! endpoints have no smaller truthful version to hand a viewer.

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

/// The one record `$node` answers, read as an owner.
fn asked(store: &Store) -> (String, Value) {
    let mut session = Session::new(store);
    session.sign_in("root", PASSWORD).ok();
    let outcomes = session.run("SELECT * FROM $node;").unwrap();
    let Some(Outcome::Records { records, .. }) = outcomes.first() else {
        panic!("not a read: {outcomes:?}");
    };
    assert_eq!(records.len(), 1, "a node is one node");
    (records[0].0.to_string(), records[0].1.clone())
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

#[test]
fn a_node_answers_its_identity_through_the_ordinary_read_path() {
    let store = closed(&backend());
    let (id, record) = asked(&store);

    // Sixteen bytes, rendered as the hex an operator pastes into a search.
    assert_eq!(id.len(), 32, "{id}");
    assert!(id.chars().all(|found| found.is_ascii_hexdigit()), "{id}");

    let Value::Object(fields) = record else {
        panic!("not an object: {record:?}");
    };
    // Every field F5 names, and the roles a node standing alone actually holds:
    // it serves and it writes, and it coordinates with nobody.
    assert_eq!(
        fields.get("roles"),
        Some(&Value::Array(vec![
            Value::from("serving"),
            Value::from("writable"),
        ]))
    );
    // Absent, and asserted so: see `configuration.rs` for why a constant field
    // that reads as a claim about cluster membership was worse than no field.
    assert!(
        !fields.contains_key("membership"),
        "membership is answered again: {fields:?}"
    );
    assert_eq!(fields.get("endpoints"), Some(&Value::Array(Vec::new())));
}

#[test]
fn a_node_names_the_build_it_is_running_and_not_only_the_family_it_belongs_to() {
    // A pre-release and the release that follows it carry the same three
    // numbers, so `version` alone cannot tell an operator which one they are
    // holding. `build` is the field that can, and the two are asserted together
    // because the failure being prevented is one of them going missing while
    // the other keeps the test green.
    let store = closed(&backend());
    let (_, record) = asked(&store);
    let Value::Object(fields) = record else {
        panic!("not an object: {record:?}");
    };

    let build = fields.get("build").expect("a node names its build");
    assert_eq!(build, &Value::from(tessari_storage::BUILD_VERSION));

    let Some(Value::String(ordered)) = fields.get("version") else {
        panic!("a node names its ordered version: {fields:?}");
    };
    let Value::String(exact) = build else {
        panic!("the build is not text: {build:?}");
    };
    assert!(
        exact.starts_with(ordered.as_str()),
        "the build {exact} and the version {ordered} disagree about the numbers"
    );
}

#[test]
fn the_identity_survives_a_restart() {
    // The criterion itself. Re-*opening* rather than re-reading, because a
    // re-read would pass with the id held in memory and never written down.
    let held = backend();
    let first = closed(&held);
    let (before, _) = asked(&first);
    drop(first);

    let second = Store::open(Arc::clone(&held)).unwrap();
    let (after, _) = asked(&second);
    assert_eq!(before, after);
}

#[test]
fn two_stores_do_not_share_an_identity() {
    // The check that keeps the test above from passing on a constant. Without
    // it, an implementation returning a fixed id satisfies "the same after a
    // restart" perfectly.
    let (first, second) = (closed(&backend()), closed(&backend()));
    assert_ne!(asked(&first).0, asked(&second).0);
}

#[test]
fn a_node_answers_without_a_namespace_or_a_database_selected() {
    // A node is not in a database, so asking about one must not require having
    // selected a tenancy. This says so rather than leaving it to the fact that
    // the helper above happens not to `USE` anything.
    let store = closed(&backend());
    let mut session = Session::new(&store);
    session.sign_in("root", PASSWORD).ok();
    assert!(session.namespace().is_none());
    assert!(session.database().is_none());
    assert!(session.run("SELECT * FROM $node;").is_ok());
}

#[test]
fn a_viewer_is_refused_rather_than_passed_over_by_an_empty_grant_check() {
    // The vacuity guard. `$node` names no table, so a rule shaped "every table
    // it names is granted" is true of it for a reason unrelated to permission.
    let store = closed(&backend());
    {
        let mut owner = Session::new(&store);
        owner.sign_in("root", PASSWORD).unwrap();
        owner
            .run(&format!(
                "DEFINE USER ada ROLE viewer PASSWORD '{PASSWORD}';"
            ))
            .unwrap();
    }
    let mut viewer = Session::new(&store);
    viewer.sign_in("ada", PASSWORD).unwrap();
    let refused = viewer.run("SELECT * FROM $node;").unwrap_err().to_string();
    // Refused for what the statement *needs*, not for a table it failed to name
    // — which is the difference between a rule and an emptiness.
    assert!(refused.contains("operate"), "{refused}");

    // And the same refusal for the plan, which would otherwise report the
    // source through a statement the caller may not run.
    assert!(viewer.run("EXPLAIN SELECT * FROM $node;").is_err());
}

#[test]
fn an_anonymous_session_reads_the_node_of_an_open_store() {
    // The other half of the permission rule, and the one that keeps an empty
    // store usable: a store with no users is open, and `$node` is not an
    // exception to that in either direction.
    let store = Store::open(backend()).unwrap();
    let mut session = Session::new(&store);
    assert!(session.run("SELECT * FROM $node;").is_ok());
}

#[test]
fn a_parameter_called_node_is_still_the_callers_own() {
    // The alternative ADR-0018's amendment rejected was a reserved parameter
    // name, refused because a caller may legitimately supply one called `node`
    // and the shadowing would be decided by evaluation order. `$node` is read as
    // a source only in the `FROM` position, so this must still work.
    let store = Store::open(backend()).unwrap();
    let mut session = Session::new(&store);
    session
        .run(
            "DEFINE NAMESPACE prod; USE NAMESPACE prod;\n\
             DEFINE DATABASE shop; USE DATABASE shop;\n\
             DEFINE COLLECTION readings;\n\
             CREATE readings:1 = { at: 'kitchen' };\n\
             CREATE readings:2 = { at: 'hall' };",
        )
        .unwrap();
    let outcomes = session
        .run_with(
            "SELECT * FROM readings WHERE at = $node;",
            &tessari_ql::Parameters::from([("node".to_owned(), Value::from("hall"))]),
        )
        .unwrap();
    let Some(Outcome::Records { records, .. }) = outcomes.first() else {
        panic!("not a read: {outcomes:?}");
    };
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].0.to_string(), "2");
}

#[test]
fn the_plan_for_a_node_read_names_the_source_rather_than_a_table() {
    let store = closed(&backend());
    let mut session = Session::new(&store);
    session.sign_in("root", PASSWORD).ok();
    let outcomes = session.run("EXPLAIN SELECT * FROM $node;").unwrap();
    let Some(Outcome::Value(Value::Object(plan))) = outcomes.first() else {
        panic!("not a plan: {outcomes:?}");
    };
    assert_eq!(plan.get("source"), Some(&Value::from("node")));
    assert_eq!(plan.get("access"), Some(&Value::from("record")));
    assert!(plan.get("table").is_none(), "a node is not a table");
}

// ------------------------------------ replication turned on after the fact

/// G024 **S2.2**: switching replication on for a namespace that already holds
/// data brings its history across, **with no repair step**.
///
/// The comparison is record by record and not by count, because a count is the
/// assertion a partial replay passes. Everything written before the `ALTER` is
/// there afterwards, and so is everything written after it — the interesting
/// half is the first, since that is the data the policy did not exist to cover
/// when it was written.
///
/// **Nothing in the alter path redistributes anything, and that is the claim.**
/// A design whose replicas hold data rather than a history has to move that
/// data, and needs a repair pass afterwards to find what the move missed. Here
/// the log holds the history, so a node that begins replicating replays it from
/// origin and the statement has nothing to do but record the policy. This test
/// is what says so — the alternative would be believing a design note.
#[test]
fn replication_turned_on_later_brings_the_whole_history_with_it() {
    let leader_backend = backend();
    let leader = Store::open(Arc::clone(&leader_backend)).unwrap();

    let mut session = Session::new(&leader);
    session
        .run(
            "DEFINE NAMESPACE prod; USE NAMESPACE prod;\n\
             DEFINE DATABASE shop; USE DATABASE shop;\n\
             DEFINE COLLECTION orders;",
        )
        .unwrap();

    // Written while the namespace said nothing about replication at all.
    for id in 1..=4 {
        session
            .run(&format!("CREATE orders:{id} = {{ total: {id} }};"))
            .unwrap();
    }
    session
        .run("UPDATE orders:2 SET total = 99;\nDELETE orders:3;")
        .unwrap();

    session
        .run("ALTER NAMESPACE prod REPLICATION FACTOR 2;")
        .unwrap();

    // And written after it, so the replay has to cross the statement rather
    // than stop at it.
    session.run("CREATE orders:5 = { total: 5 };").unwrap();

    // A follower that begins subscribing has nothing but the log.
    let follower_backend = backend();
    let follower = Store::open(Arc::clone(&follower_backend)).unwrap();
    let applied = crate::replay(&leader, &follower);
    assert!(applied > 0, "the leader wrote nothing to replay");

    // Record by record, as raw bytes: "the same data" and "the same bytes" are
    // different claims and a replica has to make the second one.
    let records = |backend: &Arc<dyn KvBackend>| {
        backend
            .scan(&tessari_kv::ScanRequest::new(
                tessari_kv::Keyspace::DATA,
                tessari_kv::KeyRange::all(),
            ))
            .unwrap()
    };
    // Apart from the version each record is keyed by: the replay walks one log
    // and then the next, which is not the order the commits interleaved in, and
    // a node numbers its own records (Q-614, Q-627).
    assert_eq!(
        crate::unversioned(&records(&leader_backend)),
        crate::unversioned(&records(&follower_backend)),
        "the follower is missing or differs on a record the leader holds"
    );

    // And the same comparison in the reader's own terms, because the keyspace
    // above also carries the catalog: a replay that brought the declarations
    // across and none of the rows would satisfy a byte comparison of an empty
    // table, and this is the assertion it could not pass.
    let orders = |store: &Store| {
        let mut session = Session::new(store);
        let outcomes = session
            .run("USE NAMESPACE prod; USE DATABASE shop; SELECT * FROM orders;")
            .unwrap();
        let Some(Outcome::Records { records, .. }) = outcomes.last() else {
            panic!("not a read: {outcomes:?}");
        };
        records.clone()
    };
    let held = orders(&leader);
    assert_eq!(
        held.len(),
        4,
        "four live records — one deleted, one updated in place"
    );
    assert_eq!(
        held,
        orders(&follower),
        "the follower answers a different set of records than the leader"
    );

    // The policy itself crossed too, because a namespace definition is a
    // catalog record and the catalog replicates through the same apply path.
    let mut reading = Session::new(&follower);
    let outcomes = reading
        .run("USE NAMESPACE prod; INFO FOR NAMESPACE;")
        .unwrap();
    let Some(Outcome::Value(Value::Object(fields))) = outcomes.last() else {
        panic!("expected a report: {outcomes:?}");
    };
    assert_eq!(fields.get("replication"), Some(&Value::from(2_i64)));
}
