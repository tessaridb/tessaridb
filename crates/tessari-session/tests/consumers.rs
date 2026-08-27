//! Declaring a consumer, forgetting one, and asking about them.
//!
//! The grammar's own tests are in `tessari-ql`. What is checked here is
//! everything the parser cannot know: that the destination exists, that the
//! format is one this store reads, that the name is claimed once, that the
//! declaration survives a reopen, and that a caller with authority over one
//! database cannot point a background writer at another's table.
//!
//! # Two different guards, and only one of them is new
//!
//! A caller can be held out of a table two ways, and a consumer has to be held
//! out both ways, because it keeps writing after the session that issued it has
//! gone.
//!
//! - A **tenancy-scoped** user — `ada ON prod.shop` — is stopped by resolving
//!   the destination: `resolve_table` asks whether the caller's tenancy permits
//!   the database, and it already did that before this statement existed.
//! - A **grant-governed** user is not, and this is the one `DEFINE CONSUMER`
//!   appears in `reach.rs` for. Grants are checked against the tables a
//!   statement *names*, so a statement naming none passes the loop
//!   **vacuously** — the shape that let a grant-governed owner take a whole
//!   backup. Reporting the destination is what closes it.
//!
//! Both are asserted below, and the second was checked by removing the arm and
//! watching the test go green: it did, which is how the first draft of this file
//! came to be testing the wrong thing.

#![allow(clippy::panic, clippy::unwrap_used)]

use std::sync::Arc;

use tessari_kv::{KvBackend, MemoryBackend};
use tessari_session::{Outcome, Session};
use tessari_storage::Store;
use tessari_types::Value;

const PASSWORD: &str = "correct horse battery";

/// A declaration against `prod.shop.orders`, ready to be varied.
const DECLARE: &str = "DEFINE CONSUMER orders_in \
     FROM 'broker-1:9092' TOPIC 'orders' GROUP 'shop-orders' \
     FORMAT json INTO orders IDENTITY order_id \
     MAP amount AS total, placed.at AS placed_at \
     ON FAILURE quarantine;";

fn backend() -> Arc<dyn KvBackend> {
    Arc::new(MemoryBackend::new()) as Arc<dyn KvBackend>
}

/// A store holding `prod.shop.orders`, with nobody declared.
fn shaped(backend: &Arc<dyn KvBackend>) -> Store {
    let store = Store::open(Arc::clone(backend)).unwrap();
    let mut session = Session::new(&store);
    session
        .run(
            "DEFINE NAMESPACE prod; USE NAMESPACE prod; \
             DEFINE DATABASE shop; USE DATABASE shop; DEFINE TABLE orders;",
        )
        .unwrap();
    store
}

/// A session selected onto `prod.shop`.
fn inside(store: &Store) -> Session<'_> {
    let mut session = Session::new(store);
    session
        .run("USE NAMESPACE prod; USE DATABASE shop;")
        .unwrap();
    session
}

/// The object an `INFO` answered with.
fn reported(outcome: Vec<Outcome>) -> std::collections::BTreeMap<String, Value> {
    let Some(Outcome::Value(Value::Object(fields))) = outcome.last() else {
        panic!("the report is not an object: {outcome:?}");
    };
    fields.clone()
}

/// A nested field of a report.
fn group<'a>(
    report: &'a std::collections::BTreeMap<String, Value>,
    name: &str,
) -> &'a std::collections::BTreeMap<String, Value> {
    let Some(Value::Object(held)) = report.get(name) else {
        panic!("{name} is not a group in {report:?}");
    };
    held
}

#[test]
fn a_declared_consumer_is_read_back_field_for_field() {
    let backend = backend();
    let store = shaped(&backend);
    let mut session = inside(&store);
    session.run(DECLARE).unwrap();

    let report = reported(session.run("INFO FOR CONSUMER orders_in;").unwrap());
    let declared = group(&report, "declared");
    assert_eq!(declared.get("name"), Some(&Value::from("orders_in")));
    assert_eq!(declared.get("topic"), Some(&Value::from("orders")));
    assert_eq!(declared.get("group"), Some(&Value::from("shop-orders")));
    assert_eq!(declared.get("format"), Some(&Value::from("json")));
    assert_eq!(declared.get("identity"), Some(&Value::from("order_id")));
    assert_eq!(declared.get("destination"), Some(&Value::from("orders")));
    assert_eq!(declared.get("on_failure"), Some(&Value::from("quarantine")));
    let Some(Value::Array(brokers)) = declared.get("brokers") else {
        panic!("no brokers");
    };
    assert_eq!(brokers, &vec![Value::from("broker-1:9092")]);
    let Some(Value::Array(mapping)) = declared.get("mapping") else {
        panic!("no mapping");
    };
    assert_eq!(mapping.len(), 2);
}

#[test]
fn the_declaration_survives_a_reopen_and_the_running_half_does_not() {
    // The split ADR-0023 §6 makes, asserted as one property because the two
    // halves are only meaningful against each other: what a restart keeps is the
    // instruction, and what it drops is the claim that something is carrying it
    // out. A `running` flag that survived would be a process reporting a thread
    // that died with the last one.
    let backend = backend();
    {
        let store = shaped(&backend);
        inside(&store).run(DECLARE).unwrap();
    }
    let store = Store::open(Arc::clone(&backend)).unwrap();
    let report = reported(inside(&store).run("INFO FOR CONSUMER orders_in;").unwrap());
    assert_eq!(
        group(&report, "declared").get("name"),
        Some(&Value::from("orders_in")),
        "the declaration did not survive the reopen"
    );
    assert_eq!(
        group(&report, "running").get("here"),
        Some(&Value::Bool(false)),
        "a reopened store claims to be running a consumer"
    );
}

#[test]
fn the_report_states_the_delivery_guarantee_where_it_is_configured() {
    // Not decoration. The system that has shipped this feature longest documents
    // its guarantee in a guide and a design proposal, and the page somebody reads
    // while configuring a consumer says messages are "only counted once" — which
    // reads as exactly-once to anybody in a hurry. This is the fix for that,
    // and it is asserted so it cannot be quietly dropped as noise.
    let backend = backend();
    let store = shaped(&backend);
    let mut session = inside(&store);
    session.run(DECLARE).unwrap();

    let report = reported(session.run("INFO FOR CONSUMER orders_in;").unwrap());
    let guarantees = group(&report, "guarantees");
    assert_eq!(
        guarantees.get("delivery"),
        Some(&Value::from("at-least-once"))
    );
    let Some(Value::String(refused)) = guarantees.get("exactly_once") else {
        panic!("the report does not say what it refuses");
    };
    assert!(refused.contains("not offered"), "{refused}");
    let Some(Value::String(schema)) = guarantees.get("schema") else {
        panic!("no schema statement");
    };
    assert!(schema.contains("never inferred"), "{schema}");
}

#[test]
fn a_destination_that_does_not_exist_is_refused_at_declaration() {
    // The race the two-object design cannot close, closed by construction: a
    // consumer whose destination is resolved as a field cannot start before its
    // destination exists, because there is no consumer until it does.
    let backend = backend();
    let store = shaped(&backend);
    let mut session = inside(&store);
    let failure = session
        .run(&DECLARE.replace("INTO orders", "INTO nowhere"))
        .unwrap_err()
        .to_string();
    assert!(failure.contains("table"), "{failure}");
    assert!(
        session.run("INFO FOR CONSUMERS;").is_ok(),
        "the failed declaration broke the listing"
    );
    let report = reported(session.run("INFO FOR CONSUMERS;").unwrap());
    let Some(Value::Array(listed)) = report.get("consumers") else {
        panic!("no listing");
    };
    assert!(listed.is_empty(), "a refused declaration was stored anyway");
}

#[test]
fn a_format_this_store_cannot_read_is_refused_where_it_is_written() {
    let backend = backend();
    let store = shaped(&backend);
    let failure = inside(&store)
        .run(&DECLARE.replace("FORMAT json", "FORMAT protobuf"))
        .unwrap_err()
        .to_string();
    assert!(failure.contains("protobuf"), "{failure}");
    assert!(failure.contains("format"), "{failure}");
}

#[test]
fn one_record_field_may_not_be_mapped_twice() {
    // Two message fields landing on one record field is a mapping whose result
    // depends on which pair happened to be applied last — a per-message coin
    // flip that no test would ever catch in production.
    let backend = backend();
    let store = shaped(&backend);
    let failure = inside(&store)
        .run(&DECLARE.replace(
            "MAP amount AS total, placed.at AS placed_at",
            "MAP amount AS total, gross AS total",
        ))
        .unwrap_err()
        .to_string();
    assert!(failure.contains("total"), "{failure}");
}

#[test]
fn a_name_is_claimed_once_and_if_not_exists_is_the_way_to_re_run_a_script() {
    let backend = backend();
    let store = shaped(&backend);
    let mut session = inside(&store);
    session.run(DECLARE).unwrap();
    assert!(session.run(DECLARE).is_err(), "the name was taken twice");
    session
        .run(&DECLARE.replace("CONSUMER orders_in", "CONSUMER IF NOT EXISTS orders_in"))
        .unwrap();
}

#[test]
fn dropping_forgets_it_and_dropping_an_unknown_one_says_so() {
    let backend = backend();
    let store = shaped(&backend);
    let mut session = inside(&store);
    session.run(DECLARE).unwrap();
    session.run("DROP CONSUMER orders_in;").unwrap();

    assert!(session.run("INFO FOR CONSUMER orders_in;").is_err());
    let failure = session
        .run("DROP CONSUMER orders_in;")
        .unwrap_err()
        .to_string();
    assert!(failure.contains("consumer"), "{failure}");

    // And the name is free again, which is what makes a drop a drop rather than
    // a tombstone somebody has to work around.
    session.run(DECLARE).unwrap();
}

#[test]
fn the_listing_says_which_ones_this_node_is_running() {
    let backend = backend();
    let store = shaped(&backend);
    let mut session = inside(&store);
    session.run(DECLARE).unwrap();
    session
        .run(
            &DECLARE
                .replace("orders_in", "orders_late")
                .replace("GROUP 'shop-orders'", "GROUP 'shop-orders-late'"),
        )
        .unwrap();

    let report = reported(session.run("INFO FOR CONSUMERS;").unwrap());
    let Some(Value::Array(listed)) = report.get("consumers") else {
        panic!("no listing");
    };
    assert_eq!(listed.len(), 2);
    for entry in listed {
        let Value::Object(fields) = entry else {
            panic!("not an object");
        };
        assert_eq!(
            fields.get("running"),
            Some(&Value::Bool(false)),
            "nothing has been started, so nothing is running"
        );
    }
}

#[test]
fn a_destination_dropped_under_a_consumer_is_reported_as_gone() {
    // The condition an operator is looking for when a consumer stops landing
    // anything. Omitting the field would read as a display bug rather than as
    // the answer.
    let backend = backend();
    let store = shaped(&backend);
    let mut session = inside(&store);
    session.run(DECLARE).unwrap();
    session.run("DROP TABLE orders;").unwrap();

    let report = reported(session.run("INFO FOR CONSUMER orders_in;").unwrap());
    assert_eq!(
        group(&report, "declared").get("destination"),
        Some(&Value::from("<dropped>"))
    );
}

// --------------------------------------------------------------- authorization

/// Two namespaces, a store owner, and an owner of one database in one of them.
fn peopled(store: &Store) {
    let mut session = Session::new(store);
    session
        .run(
            "DEFINE NAMESPACE secret; USE NAMESPACE secret; \
             DEFINE DATABASE vault; USE DATABASE vault; DEFINE TABLE holdings;",
        )
        .unwrap();
    session
        .run("DEFINE USER root ROLE owner PASSWORD 'correct horse battery';")
        .unwrap();
    let mut root = Session::new(store);
    root.sign_in("root", PASSWORD).unwrap();
    root.run(
        "DEFINE USER nina ON prod.shop ROLE owner PASSWORD 'correct horse battery'; \
         DEFINE USER ada ON prod.shop ROLE editor PASSWORD 'correct horse battery';",
    )
    .unwrap();
}

fn signed_in<'a>(store: &'a Store, name: &str) -> Session<'a> {
    let mut session = Session::new(store);
    session.sign_in(name, PASSWORD).unwrap();
    session
        .run("USE NAMESPACE prod; USE DATABASE shop;")
        .unwrap();
    session
}

#[test]
fn an_editor_may_not_declare_a_consumer() {
    // Declaring one is administering, not writing. It hands a broker address and
    // a group to a process that then writes into somebody's table with nobody
    // watching, which is a decision about what runs rather than about what the
    // data looks like.
    let backend = backend();
    let store = shaped(&backend);
    peopled(&store);
    let failure = signed_in(&store, "ada")
        .run(DECLARE)
        .unwrap_err()
        .to_string();
    assert!(failure.contains("administer"), "{failure}");
}

#[test]
fn an_owner_of_one_database_may_declare_a_consumer_into_their_own_table() {
    // The other half of the boundary, and the reason this is `Administer` rather
    // than `AdministerStore`: an owner of `prod.shop` should be able to say what
    // feeds `prod.shop.orders` without being the owner of the whole store.
    let backend = backend();
    let store = shaped(&backend);
    peopled(&store);
    signed_in(&store, "nina").run(DECLARE).unwrap();
}

#[test]
fn a_consumer_cannot_be_aimed_at_a_table_the_caller_may_not_write() {
    // The **tenancy** half. `nina` holds `prod.shop`, and resolving a
    // destination in `prod.payroll` is refused before anything is written.
    //
    // The sibling is a **database** and not a namespace, and that is not a
    // weaker test — it is the only one that exists. A table reference is
    // `database.table`, so `secret.vault.holdings` does not parse and a second
    // namespace cannot be named as a destination at all. The first draft of this
    // test used one and passed on a **parse error**, which is a test that would
    // have gone on passing after every guard was deleted.
    let backend = backend();
    let store = shaped(&backend);
    let mut setup = Session::new(&store);
    setup
        .run(
            "USE NAMESPACE prod; DEFINE DATABASE payroll; USE DATABASE payroll; \
             DEFINE TABLE salaries;",
        )
        .unwrap();
    peopled(&store);

    let failure = signed_in(&store, "nina")
        .run(&DECLARE.replace("INTO orders", "INTO payroll.salaries"))
        .unwrap_err()
        .to_string();
    assert!(
        !failure.contains("expected"),
        "refused by the parser rather than by the permission check, so this test \
         would pass with the guard deleted: {failure}"
    );
    assert!(
        failure.contains("payroll"),
        "the refusal does not name the tenancy that was reached for: {failure}"
    );

    // And nothing was declared, so the refusal is not a half-write.
    let mut root = Session::new(&store);
    root.sign_in("root", PASSWORD).unwrap();
    let report = reported(root.run("INFO FOR CONSUMERS;").unwrap());
    let Some(Value::Array(listed)) = report.get("consumers") else {
        panic!("no listing");
    };
    assert!(listed.is_empty(), "a refused declaration was stored");
}

#[test]
fn the_store_owner_may_aim_a_consumer_at_any_of_it() {
    // The other side of the boundary, so the test above is proved to be about
    // *who* rather than about the statement being broken.
    let backend = backend();
    let store = shaped(&backend);
    let mut setup = Session::new(&store);
    setup
        .run(
            "USE NAMESPACE prod; DEFINE DATABASE payroll; USE DATABASE payroll; \
             DEFINE TABLE salaries;",
        )
        .unwrap();
    peopled(&store);

    let mut root = Session::new(&store);
    root.sign_in("root", PASSWORD).unwrap();
    root.run("USE NAMESPACE prod; USE DATABASE shop;").unwrap();
    root.run(&DECLARE.replace("INTO orders", "INTO payroll.salaries"))
        .unwrap();
}

#[test]
fn asking_about_a_consumer_needs_more_than_being_signed_in() {
    // It refuses rather than filters, for `INFO FOR USER`'s reason: a broker
    // address, a group name and a position have no smaller truthful form to hand
    // somebody who may only read.
    let backend = backend();
    let store = shaped(&backend);
    let mut session = inside(&store);
    session.run(DECLARE).unwrap();
    peopled(&store);

    let mut root = Session::new(&store);
    root.sign_in("root", PASSWORD).unwrap();
    root.run("DEFINE USER vic ON prod.shop ROLE viewer PASSWORD 'correct horse battery';")
        .unwrap();

    assert!(
        signed_in(&store, "vic")
            .run("INFO FOR CONSUMER orders_in;")
            .is_err()
    );
    assert!(signed_in(&store, "vic").run("INFO FOR CONSUMERS;").is_err());
    signed_in(&store, "nina")
        .run("INFO FOR CONSUMER orders_in;")
        .unwrap();
}

#[test]
fn a_grant_governed_caller_may_only_feed_the_tables_they_were_granted() {
    // The half that `reach.rs` actually carries, and the reason the arm exists.
    //
    // `pat` is an owner of the whole store, so no tenancy check holds them back;
    // what narrows them is a grant. Grants are checked against the tables a
    // statement **names**, so before the arm was added `DEFINE CONSUMER` named
    // none and the check passed over it — the same vacuous pass that let a
    // grant-governed owner take a whole backup.
    //
    // Verified by removing the arm and re-running: this assertion fails, and the
    // one above it does not. That is what distinguishes the two guards.
    let backend = backend();
    let store = shaped(&backend);
    let mut setup = Session::new(&store);
    setup
        .run("USE NAMESPACE prod; USE DATABASE shop; DEFINE TABLE salaries;")
        .unwrap();
    setup
        .run("DEFINE USER root ROLE owner PASSWORD 'correct horse battery';")
        .unwrap();

    let mut root = Session::new(&store);
    root.sign_in("root", PASSWORD).unwrap();
    root.run("USE NAMESPACE prod; USE DATABASE shop;").unwrap();
    root.run("DEFINE USER pat ROLE owner PASSWORD 'correct horse battery';")
        .unwrap();
    root.run("GRANT read, write ON orders TO pat;").unwrap();

    // Granted the destination: accepted.
    let mut pat = Session::new(&store);
    pat.sign_in("pat", PASSWORD).unwrap();
    pat.run("USE NAMESPACE prod; USE DATABASE shop;").unwrap();
    pat.run(DECLARE).unwrap();

    // Not granted it: refused, and the refusal names the table and the verb.
    let mut pat = Session::new(&store);
    pat.sign_in("pat", PASSWORD).unwrap();
    pat.run("USE NAMESPACE prod; USE DATABASE shop;").unwrap();
    let failure = pat
        .run(
            &DECLARE
                .replace("INTO orders", "INTO salaries")
                .replace("CONSUMER orders_in", "CONSUMER wages_in"),
        )
        .unwrap_err()
        .to_string();
    assert!(
        failure.contains("salaries") && failure.contains("write"),
        "a granted caller aimed a consumer at a table they hold no grant on: {failure}"
    );
}
