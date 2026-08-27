//! `EXPLAIN` — the plan a read would take, without taking it.
//!
//! The planner has chosen by what an index removes since it existed; what was
//! missing is that nobody outside could see which it chose. A decision nobody
//! can look at is a decision nobody can debug, and — the reason it matters here
//! — one no test can assert without timing it.
//!
//! Two of these tests are about the *rule* rather than the report: the choice
//! must not depend on the order the conjuncts were written in, and the answer
//! must be the same under every plan. The second is the store's governing rule
//! seen from the outside: an index changes what a read costs and never what it
//! answers.

#![allow(clippy::panic, clippy::unwrap_used, clippy::indexing_slicing)]

use std::sync::Arc;

use tessari_kv::{KvBackend, MemoryBackend};
use tessari_session::{Outcome, Session};
use tessari_storage::Store;
use tessari_types::{RecordId, Value};

const PASSWORD: &str = "correct horse battery";

fn store() -> Store {
    let backend = Arc::new(MemoryBackend::new()) as Arc<dyn KvBackend>;
    Store::open(backend).unwrap()
}

fn ready(store: &Store) -> Session<'_> {
    let mut session = Session::new(store);
    session
        .run(
            "DEFINE NAMESPACE prod; USE NAMESPACE prod;\n\
             DEFINE DATABASE shop; USE DATABASE shop;\n\
             DEFINE TABLE users;\n\
             CREATE users:1 = { email: 'ada@example.com', city: 'Paris' };\n\
             CREATE users:2 = { email: 'grace@example.com', city: 'Paris' };\n\
             CREATE users:3 = { email: 'alan@example.com', city: 'Lyon' };",
        )
        .unwrap();
    session
}

/// One field of the plan a read explains under.
fn plan(session: &mut Session<'_>, script: &str, field: &str) -> String {
    let outcomes = session.run(script).unwrap();
    let Some(Outcome::Value(Value::Object(fields))) = outcomes.last() else {
        panic!("a plan answered with {:?}", outcomes.last());
    };
    format!("{:?}", fields.get(field).unwrap_or(&Value::None))
}

fn ids(session: &mut Session<'_>, script: &str) -> Vec<RecordId> {
    let outcomes = session.run(script).unwrap();
    let mut found: Vec<RecordId> = outcomes
        .last()
        .unwrap()
        .records()
        .unwrap()
        .iter()
        .map(|(id, _)| id.clone())
        .collect();
    found.sort();
    found
}

#[test]
fn a_read_no_index_serves_explains_as_a_scan() {
    let store = store();
    let mut session = ready(&store);
    assert_eq!(
        plan(
            &mut session,
            "EXPLAIN SELECT * FROM users WHERE city = 'Paris';",
            "access"
        ),
        r#"String("scan")"#
    );
    assert_eq!(
        plan(&mut session, "EXPLAIN SELECT * FROM users;", "access"),
        r#"String("scan")"#
    );
}

#[test]
fn an_index_served_read_names_the_index_and_the_shape() {
    let store = store();
    let mut session = ready(&store);
    session
        .run("DEFINE INDEX by_city ON users FIELDS city;")
        .unwrap();
    let script = "EXPLAIN SELECT * FROM users WHERE city = 'Paris';";
    assert_eq!(plan(&mut session, script, "access"), r#"String("index")"#);
    assert_eq!(plan(&mut session, script, "index"), r#"String("by_city")"#);
    assert_eq!(plan(&mut session, script, "shape"), r#"String("equality")"#);
    assert_eq!(plan(&mut session, script, "table"), r#"String("users")"#);
    // No ceiling was free to learn on a secondary index, so none is printed. A
    // number this store cannot know is a number it will not print.
    assert_eq!(plan(&mut session, script, "at_most"), "None");
}

#[test]
fn it_names_the_more_selective_of_two_usable_indexes() {
    // The whole point of the ranking, asserted on the reported plan rather than
    // on how long anything took. `email` is unique, so an equality on it is at
    // most one record; `city` is not, so its ceiling is unknown.
    let store = store();
    let mut session = ready(&store);
    session
        .run(
            "DEFINE INDEX by_city ON users FIELDS city;\n\
             DEFINE INDEX by_email ON users FIELDS email UNIQUE;",
        )
        .unwrap();
    let script = "EXPLAIN SELECT * FROM users WHERE city = 'Paris' AND email = 'ada@example.com';";
    assert_eq!(plan(&mut session, script, "index"), r#"String("by_email")"#);
    assert_eq!(plan(&mut session, script, "at_most"), "Number(Integer(1))");
}

#[test]
fn the_order_the_conjuncts_were_written_in_does_not_decide() {
    // The defect the planner exists to prevent: taking the first servable
    // clause makes the cost of a read depend on where its author happened to
    // put a word.
    let store = store();
    let mut session = ready(&store);
    session
        .run(
            "DEFINE INDEX by_city ON users FIELDS city;\n\
             DEFINE INDEX by_email ON users FIELDS email UNIQUE;",
        )
        .unwrap();
    for script in [
        "EXPLAIN SELECT * FROM users WHERE city = 'Paris' AND email = 'ada@example.com';",
        "EXPLAIN SELECT * FROM users WHERE email = 'ada@example.com' AND city = 'Paris';",
    ] {
        assert_eq!(
            plan(&mut session, script, "index"),
            r#"String("by_email")"#,
            "{script}"
        );
    }
}

#[test]
fn the_answer_is_the_same_under_every_plan() {
    // The store's governing rule, seen from outside: an index changes what a
    // read costs and never what it answers. Four stores, four plans, one answer
    // — record for record, not merely in count.
    let read = "SELECT * FROM users WHERE city = 'Paris' AND email = 'ada@example.com';";
    let mut answers = Vec::new();
    let mut plans = Vec::new();
    for declarations in [
        "",
        "DEFINE INDEX by_city ON users FIELDS city;",
        "DEFINE INDEX by_email ON users FIELDS email UNIQUE;",
        "DEFINE INDEX by_city ON users FIELDS city;\n\
         DEFINE INDEX by_email ON users FIELDS email UNIQUE;",
    ] {
        let store = store();
        let mut session = ready(&store);
        if !declarations.is_empty() {
            session.run(declarations).unwrap();
        }
        plans.push(plan(&mut session, &format!("EXPLAIN {read}"), "index"));
        answers.push(ids(&mut session, read));
    }
    assert_eq!(answers[0], vec![RecordId::Int(1)]);
    for found in &answers {
        assert_eq!(found, &answers[0], "a plan changed the answer: {plans:?}");
    }
    // …and the plans really did differ, so the equality above is not four scans
    // agreeing with each other.
    assert_eq!(
        plans,
        vec![
            "None".to_owned(),
            r#"String("by_city")"#.to_owned(),
            r#"String("by_email")"#.to_owned(),
            r#"String("by_email")"#.to_owned(),
        ]
    );
}

#[test]
fn explaining_a_read_does_not_run_it() {
    // A diagnostic with an effect is not a diagnostic. Asserted through the
    // change feed's own sequence rather than by looking at the records, because
    // a read leaves those alone whether or not it happened.
    let store = store();
    let mut session = ready(&store);
    let before = store.committed_tail().unwrap();
    session
        .run("EXPLAIN SELECT * FROM users WHERE city = 'Paris';")
        .unwrap();
    assert_eq!(store.committed_tail().unwrap(), before);
    assert_eq!(ids(&mut session, "SELECT * FROM users;").len(), 3);
}

#[test]
fn a_point_read_and_a_walk_say_what_they_are() {
    let store = store();
    let mut session = ready(&store);
    assert_eq!(
        plan(&mut session, "EXPLAIN SELECT * FROM users:1;", "access"),
        r#"String("record")"#
    );
    session
        .run("DEFINE TABLE follows EDGE; RELATE users:1->follows->users:2;")
        .unwrap();
    assert_eq!(
        plan(
            &mut session,
            "EXPLAIN SELECT * FROM users:1->follows->users;",
            "access"
        ),
        r#"String("graph")"#
    );
}

#[test]
fn a_materialised_source_says_it_is_one() {
    // Honest rather than complete: the inner read has a plan of its own and this
    // report does not carry it. One structure covering both is R-2, and it
    // belongs with the note channel rather than here — so the word says what
    // this read did and claims nothing about what the read inside it did.
    let store = store();
    let mut session = ready(&store);
    assert_eq!(
        plan(
            &mut session,
            "EXPLAIN SELECT * FROM (SELECT * FROM users LIMIT 10);",
            "access"
        ),
        r#"String("materialised")"#
    );
}

#[test]
fn it_needs_exactly_the_permission_the_read_needs() {
    // An `EXPLAIN` that named the index serving a table the caller may not read
    // would be a metadata disclosure wearing a diagnostic's clothes — and the
    // shape it would take is the one a backup took until it was refused by name:
    // a statement naming no table passes a grant check vacuously.
    let store = store();
    let mut session = ready(&store);
    session
        .run(
            "DEFINE TABLE secrets;\n\
             CREATE secrets:1 = { held: 'x' };\n\
             DEFINE INDEX by_held ON secrets FIELDS held;\n\
             DEFINE USER root ROLE owner PASSWORD 'correct horse battery';",
        )
        .unwrap();
    let mut root = Session::new(&store);
    root.sign_in("root", PASSWORD).unwrap();
    root.run("DEFINE USER ada ON prod.shop ROLE editor PASSWORD 'correct horse battery';")
        .unwrap();
    root.run("USE NAMESPACE prod; USE DATABASE shop; GRANT read ON users TO ada;")
        .unwrap();

    let mut ada = Session::new(&store);
    ada.sign_in("ada", PASSWORD).unwrap();
    ada.run("USE NAMESPACE prod; USE DATABASE shop;").unwrap();
    // Granted on `users`: the read runs and so does its explanation.
    assert_eq!(
        plan(
            &mut ada,
            "EXPLAIN SELECT * FROM users WHERE city = 'Paris';",
            "table"
        ),
        r#"String("users")"#
    );
    // Not granted on `secrets`: neither does.
    let refused = ada.run("EXPLAIN SELECT * FROM secrets WHERE held = 'x';");
    assert!(
        refused.is_err(),
        "an ungranted table's plan was disclosed: {refused:?}"
    );
    assert!(ada.run("SELECT * FROM secrets;").is_err());
}

#[test]
fn only_a_read_can_be_explained() {
    let store = store();
    let mut session = ready(&store);
    for script in [
        "EXPLAIN CREATE users:9 = { email: 'x' };",
        "EXPLAIN DELETE users:1;",
        "EXPLAIN;",
    ] {
        let refused = session.run(script);
        assert!(refused.is_err(), "{script} was accepted: {refused:?}");
    }
}
