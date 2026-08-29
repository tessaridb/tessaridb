//! The two audit questions, and the only evidence that either is answered.
//!
//! # What is actually being tested
//!
//! Not that `INFO FOR ACCESS TO TABLE` produces a plausible list. A report about
//! permissions is the one report nobody can sanity-check by looking at it — that
//! is why it exists — so "looks right" is not available as a verdict, and a
//! report that quietly drifted from the rules it describes would read exactly
//! like a correct one.
//!
//! So the answer is checked against **what actually happens**. For every user
//! the report names and every table in the store, the test signs in as that user
//! and attempts the read and the write itself, and asserts the report said so.
//! An exhaustive cross product rather than a sample, because the interesting
//! cell is always the one nobody thought to look at.
//!
//! # Why that is not circular
//!
//! It would be if the report and the attempt met somewhere above the decision.
//! They do not: the report asks `Session::authorize` with a statement it built,
//! and the test asks `Session::run` with a statement it typed, which parses and
//! then reaches the same check from the other side. A report re-deriving
//! reachability from grants would fail this test the first time a rule changed
//! in one place and not the other — which is the entire failure mode being
//! guarded, and the reason the implementation asks rather than derives.

#![allow(clippy::panic, clippy::unwrap_used)]

use std::sync::Arc;

use tessari_kv::{KvBackend, MemoryBackend};
use tessari_session::{Outcome, Session};
use tessari_storage::Store;
use tessari_types::Value;

const PASSWORD: &str = "correct horse battery";

/// Every table the store holds, and every user that might reach one.
const TABLES: [&str; 2] = ["orders", "payroll"];
const USERS: [&str; 6] = ["root", "ada", "vic", "ed", "gus", "gina"];

fn store() -> Store {
    let backend = Arc::new(MemoryBackend::new()) as Arc<dyn KvBackend>;
    Store::open(backend).unwrap()
}

/// Two tenancies sharing a database name, two tables, and six people whose
/// authority over them differs on every axis the model has: role, reach,
/// declared tenancy, a set that no role spells, and a grant that bounds an
/// otherwise ordinary user to one table.
fn peopled(store: &Store) -> Session<'_> {
    let mut session = Session::new(store);
    session
        .run(
            "DEFINE NAMESPACE prod; USE NAMESPACE prod;\n\
             DEFINE DATABASE shop; USE DATABASE shop;\n\
             DEFINE TABLE orders; DEFINE TABLE payroll;\n\
             DEFINE USER root ROLE owner PASSWORD 'correct horse battery';",
        )
        .unwrap();
    let mut root = Session::new(store);
    root.sign_in("root", PASSWORD).unwrap();
    root.run(
        "USE NAMESPACE prod; USE DATABASE shop;\n\
         DEFINE USER ada ON prod.shop ROLE owner PASSWORD 'correct horse battery';\n\
         DEFINE USER vic ON prod.shop ROLE viewer PASSWORD 'correct horse battery';\n\
         DEFINE USER ed ON prod.shop ROLE editor PASSWORD 'correct horse battery';\n\
         DEFINE USER gus ON prod.shop AUTHORITIES manage PASSWORD 'correct horse battery';\n\
         DEFINE USER gina ON prod.shop ROLE editor PASSWORD 'correct horse battery';\n\
         GRANT read, write ON orders TO gina;",
    )
    .unwrap();
    root
}

/// The report, as a map from user name to `(read, write)`.
fn reported(session: &mut Session<'_>, table: &str) -> Vec<(String, bool, bool)> {
    let outcomes = session
        .run(&format!("INFO FOR ACCESS TO TABLE {table};"))
        .unwrap();
    let Some(Outcome::Value(Value::Object(report))) = outcomes.last() else {
        panic!("expected a report, got {outcomes:?}");
    };
    assert_eq!(
        report.get("table"),
        Some(&Value::from(table)),
        "the report named another table: {report:?}"
    );
    let Some(Value::Array(rows)) = report.get("access") else {
        panic!("expected an access list, got {report:?}");
    };
    let mut listed: Vec<(String, bool, bool)> = rows
        .iter()
        .map(|row| {
            let Value::Object(fields) = row else {
                panic!("expected an object per row");
            };
            match (fields.get("user"), fields.get("read"), fields.get("write")) {
                (Some(Value::String(user)), Some(Value::Bool(read)), Some(Value::Bool(write))) => {
                    (user.clone(), *read, *write)
                }
                other => panic!("expected a user and two answers, found {other:?}"),
            }
        })
        .collect();
    listed.sort();
    listed
}

/// What actually happens when that user attempts it, from a session of their
/// own — the statement typed rather than built, so the two meet at the check
/// and nowhere above it.
fn attempted(store: &Store, user: &str, table: &str) -> (bool, bool) {
    let mut read = Session::new(store);
    read.sign_in(user, PASSWORD).unwrap();
    let reading = read
        .run(&format!(
            "USE NAMESPACE prod; USE DATABASE shop; SELECT * FROM {table};"
        ))
        .is_ok();
    let mut write = Session::new(store);
    write.sign_in(user, PASSWORD).unwrap();
    let writing = write
        .run(&format!(
            "USE NAMESPACE prod; USE DATABASE shop; DELETE {table}:0;"
        ))
        .is_ok();
    (reading, writing)
}

#[test]
fn the_report_matches_what_actually_happens_for_every_user_and_every_table() {
    let store = store();
    let mut root = peopled(&store);

    for table in TABLES {
        let listed = reported(&mut root, table);
        // Everybody the caller administers appears, whether or not they reach
        // it: an absent row would say *cannot reach* and *cannot be seen* with
        // the same silence, and an audit answer has to tell those apart.
        let named: Vec<&str> = listed.iter().map(|(user, ..)| user.as_str()).collect();
        let mut expected = USERS;
        expected.sort_unstable();
        assert_eq!(named, expected, "the report on {table} named the wrong set");

        for (user, read, write) in listed {
            let (attempted_read, attempted_write) = attempted(&store, &user, table);
            assert_eq!(
                (read, write),
                (attempted_read, attempted_write),
                "{user} on {table}: the report said ({read}, {write}) and the store did \
                 ({attempted_read}, {attempted_write})"
            );
        }
    }
}

#[test]
fn the_grant_bounded_user_is_the_case_a_role_alone_would_get_wrong() {
    // `gina` is an editor, so a report reading her role would say she reaches
    // both tables. She holds a grant, and a grant bounds an editor to what it
    // names — which is why she is in the fixture and why this case is asserted
    // on its own rather than left inside the cross product above.
    let store = store();
    let mut root = peopled(&store);

    let orders = reported(&mut root, "orders");
    let payroll = reported(&mut root, "payroll");
    let hers = |listed: &[(String, bool, bool)]| {
        listed
            .iter()
            .find(|(user, ..)| user == "gina")
            .map(|(_, read, write)| (*read, *write))
            .unwrap()
    };
    assert_eq!(
        hers(&orders),
        (true, true),
        "her grant did not reach orders"
    );
    assert_eq!(hers(&payroll), (false, false), "she read past her grant");
    // And the editor beside her, who holds no grant, reaches both — without
    // which the two answers above would also be satisfied by a store that had
    // stopped answering.
    let his = |listed: &[(String, bool, bool)]| {
        listed
            .iter()
            .find(|(user, ..)| user == "ed")
            .map(|(_, read, write)| (*read, *write))
            .unwrap()
    };
    assert_eq!(his(&orders), (true, true));
    assert_eq!(his(&payroll), (true, true));
}

#[test]
fn a_tenant_of_another_namespace_reaches_nothing_here_and_is_said_to() {
    // The gate this case exists for is not the one it looks like. A user
    // declared in another namespace is stopped at `USE` — `within_tenancy`
    // reads that statement and no other, because until something selects a
    // container a namespace is only a name — so a report that handed a probe
    // the asker's selection outright would skip the gate entirely and announce
    // that a tenant of `staging` reads `prod.shop.orders`. That is precisely
    // what the first draft did, and it is why the probe now selects its way in.
    //
    // She is *listed*, because the store owner administers her: an absence
    // would say *cannot reach* and *cannot be seen* with the same silence.
    let store = store();
    let mut root = peopled(&store);
    root.run(
        "DEFINE NAMESPACE staging; USE NAMESPACE staging; DEFINE DATABASE shop;\n\
         USE DATABASE shop; DEFINE TABLE orders;\n\
         DEFINE USER nina ON staging.shop ROLE owner PASSWORD 'correct horse battery';\n\
         USE NAMESPACE prod; USE DATABASE shop;",
    )
    .unwrap();

    let listed = reported(&mut root, "orders");
    assert!(
        listed.contains(&("nina".to_owned(), false, false)),
        "a tenant of another namespace was given this table: {listed:?}"
    );
    assert!(
        listed.contains(&("vic".to_owned(), true, false)),
        "the viewer was not reported as read-only: {listed:?}"
    );

    // And the attempt agrees, which is what makes the row above a fact about
    // the store rather than about this function.
    let mut nina = Session::new(&store);
    nina.sign_in("nina", PASSWORD).unwrap();
    let refused = nina
        .run("USE NAMESPACE prod; USE DATABASE shop; SELECT * FROM orders;")
        .unwrap_err();
    assert!(
        refused.to_string().contains("outside"),
        "she was refused for some other reason: {refused}"
    );
}

#[test]
fn the_report_is_refused_to_everybody_who_does_not_administer_the_table() {
    // Its content is the permission system, so it refuses rather than filters,
    // exactly as `INFO FOR USER` does. A viewer handed a narrowed version would
    // read it as the whole account of who may do what.
    let store = store();
    let mut root = peopled(&store);
    drop(root.run("USE NAMESPACE prod; USE DATABASE shop;").unwrap());

    for who in ["vic", "ed"] {
        let mut session = Session::new(&store);
        session.sign_in(who, PASSWORD).unwrap();
        let refused = session
            .run("USE NAMESPACE prod; USE DATABASE shop; INFO FOR ACCESS TO TABLE orders;")
            .unwrap_err();
        assert!(
            refused.to_string().contains("govern"),
            "{who} was refused for some other reason: {refused}"
        );
    }

    // And an owner is not, without which the two refusals above would be
    // satisfied by a statement nobody can run.
    let mut ada = Session::new(&store);
    ada.sign_in("ada", PASSWORD).unwrap();
    ada.run("USE NAMESPACE prod; USE DATABASE shop; INFO FOR ACCESS TO TABLE orders;")
        .unwrap();
}

#[test]
fn the_other_question_still_answers_from_the_same_material() {
    // A12 asks for a statement per question. This is the second one, and it
    // already existed — asserted here so the pair is tested together rather
    // than in two files that could drift.
    let store = store();
    let mut root = peopled(&store);
    let outcomes = root.run("INFO FOR USER gina;").unwrap();
    let Some(Outcome::Value(Value::Object(report))) = outcomes.last() else {
        panic!("expected a report, got {outcomes:?}");
    };
    let Some(Value::Array(grants)) = report.get("grants") else {
        panic!("expected a grant list, got {report:?}");
    };
    assert_eq!(
        grants.len(),
        1,
        "her one grant was not reported: {grants:?}"
    );
    assert!(
        report.get("authorities").is_some(),
        "what she may reach was not reported: {report:?}"
    );
}
