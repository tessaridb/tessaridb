//! `DEFINE TOPIC CONSUMER`, `DROP TOPIC CONSUMER`, `INFO FOR TOPIC CONSUMER`, the
//! topic's `ingested_by`, and `Session::atomically` (ADR-0087).
//!
//! What the parser cannot know is checked here: that the source is a topic with
//! the named group, that the destination is in the same database (one
//! transaction is the guarantee), that `quarantine` has a group that will stop
//! handing a message out, and that the two kinds are not confused.

#![allow(clippy::panic, clippy::unwrap_used)]

use std::collections::BTreeMap;
use std::sync::Arc;

use tessari_kv::{KvBackend, MemoryBackend};
use tessari_session::{Error, Outcome, Parameters, Session};
use tessari_storage::Store;
use tessari_types::Value;

const DECLARE: &str = "DEFINE TOPIC CONSUMER orders_in FROM orders GROUP 'rows' \
     INTO order_rows IDENTITY order_id MAP amount AS total ON FAILURE quarantine;";

/// `prod.shop` with a topic, its dead letter, a group that can park a message,
/// a group that cannot, and a destination; and `prod.other` with a table.
fn shaped() -> Store {
    let store = Store::open(Arc::new(MemoryBackend::new()) as Arc<dyn KvBackend>).unwrap();
    let mut session = Session::new(&store);
    session
        .run(
            "DEFINE NAMESPACE prod; USE NAMESPACE prod; DEFINE DATABASE other; \
             USE DATABASE other; DEFINE COLLECTION elsewhere; \
             DEFINE DATABASE shop; USE DATABASE shop; \
             DEFINE TOPIC orders; DEFINE TOPIC orders_dead; DEFINE COLLECTION order_rows; \
             DEFINE GROUP 'rows' ON TOPIC orders ACK DEADLINE 30s DELIVERIES 3 \
             DEAD LETTER TO orders_dead; \
             DEFINE GROUP 'plain' ON TOPIC orders ACK DEADLINE 30s; \
             DEFINE GROUP 'counted' ON TOPIC orders ACK DEADLINE 30s DELIVERIES 3;",
        )
        .unwrap();
    store
}

fn inside(store: &Store) -> Session<'_> {
    let mut session = Session::new(store);
    session
        .run("USE NAMESPACE prod; USE DATABASE shop;")
        .unwrap();
    session
}

fn reported(outcome: &[Outcome]) -> BTreeMap<String, Value> {
    let Some(Outcome::Value(Value::Object(fields))) = outcome.last() else {
        panic!("the report is not an object: {outcome:?}");
    };
    fields.clone()
}

fn object<'a>(report: &'a BTreeMap<String, Value>, name: &str) -> &'a BTreeMap<String, Value> {
    let Some(Value::Object(held)) = report.get(name) else {
        panic!("{name} is not an object in {report:?}");
    };
    held
}

#[test]
fn a_declared_topic_consumer_is_described_with_its_guarantee() {
    let store = shaped();
    let mut session = inside(&store);
    session.run(DECLARE).unwrap();

    let report = reported(&session.run("INFO FOR TOPIC CONSUMER orders_in;").unwrap());
    let declared = object(&report, "declared");
    assert_eq!(declared.get("topic"), Some(&Value::from("orders")));
    assert_eq!(declared.get("group"), Some(&Value::from("rows")));
    assert_eq!(
        declared.get("destination"),
        Some(&Value::from("order_rows"))
    );
    assert_eq!(declared.get("on_failure"), Some(&Value::from("quarantine")));
    assert_eq!(
        object(&report, "guarantees").get("delivery"),
        Some(&Value::from("exactly once into this store"))
    );
    assert_eq!(
        object(&report, "running").get("here"),
        Some(&Value::Bool(false)),
        "nothing runs it inside a bare session"
    );

    let topic = reported(&session.run("INFO FOR TOPIC orders;").unwrap());
    let ingested = object(object(&topic, "ingested_by"), "orders_in");
    assert_eq!(ingested.get("into"), Some(&Value::from("order_rows")));
    assert_eq!(ingested.get("group"), Some(&Value::from("rows")));
}

#[test]
fn what_a_topic_consumer_names_is_checked_before_it_is_declared() {
    let store = shaped();
    let mut session = inside(&store);
    let refused = |session: &mut Session<'_>, source: &str| session.run(source).unwrap_err();

    assert!(matches!(
        refused(
            &mut session,
            &DECLARE.replace("FROM orders", "FROM order_rows")
        ),
        Error::NotATopic { .. }
    ));
    assert!(matches!(
        refused(&mut session, &DECLARE.replace("'rows'", "'nobody'")),
        Error::NoSuchGroup { .. }
    ));
    assert!(matches!(
        refused(
            &mut session,
            &DECLARE.replace("INTO order_rows", "INTO other.elsewhere")
        ),
        Error::TopicConsumerSpansDatabases { .. }
    ));
    let without_deliveries = refused(&mut session, &DECLARE.replace("'rows'", "'plain'"));
    assert!(
        matches!(&without_deliveries, Error::QuarantineNeedsDeadLetter { missing, .. } if missing.contains("DELIVERIES")),
        "{without_deliveries}"
    );
    let without_letters = refused(&mut session, &DECLARE.replace("'rows'", "'counted'"));
    assert!(
        matches!(&without_letters, Error::QuarantineNeedsDeadLetter { missing, .. } if missing.contains("DEAD LETTER")),
        "{without_letters}"
    );
    // `stop` needs neither, because it never hands a message back.
    session
        .run(
            &DECLARE
                .replace("'rows'", "'plain'")
                .replace("quarantine", "stop"),
        )
        .unwrap();
    // Nothing above claimed the name.
    session
        .run(&DECLARE.replace("orders_in", "second"))
        .unwrap();
}

#[test]
fn the_two_kinds_are_not_confused_and_dropping_forgets_it() {
    let store = shaped();
    let mut session = inside(&store);
    session.run(DECLARE).unwrap();
    session
        .run(
            "DEFINE KAFKA CONSUMER from_broker FROM 'b:9092' TOPIC 't' GROUP 'g' FORMAT json \
             INTO order_rows IDENTITY k MAP a AS b ON FAILURE stop;",
        )
        .unwrap();

    for (source, instead) in [
        (
            "DROP KAFKA CONSUMER orders_in;",
            "DROP TOPIC CONSUMER orders_in",
        ),
        (
            "INFO FOR KAFKA CONSUMER orders_in;",
            "INFO FOR TOPIC CONSUMER orders_in",
        ),
        (
            "DROP TOPIC CONSUMER from_broker;",
            "DROP KAFKA CONSUMER from_broker",
        ),
        (
            "INFO FOR TOPIC CONSUMER from_broker;",
            "INFO FOR KAFKA CONSUMER from_broker",
        ),
    ] {
        let failure = session.run(source).unwrap_err();
        assert!(
            matches!(&failure, Error::WrongConsumerKind { instead: said, .. } if said == instead),
            "{source}: {failure}"
        );
    }
    // The Kafka listing lists Kafka consumers only.
    let listing = format!("{:?}", session.run("INFO FOR KAFKA CONSUMERS;").unwrap());
    assert!(
        listing.contains("from_broker") && !listing.contains("orders_in"),
        "{listing}"
    );

    session.run("DROP TOPIC CONSUMER orders_in;").unwrap();
    assert!(matches!(
        session
            .run("INFO FOR TOPIC CONSUMER orders_in;")
            .unwrap_err(),
        Error::Unknown { .. }
    ));
    let topic = reported(&session.run("INFO FOR TOPIC orders;").unwrap());
    assert!(object(&topic, "ingested_by").is_empty(), "{topic:?}");
}

#[test]
fn atomically_commits_several_scripts_together_or_none_of_them() {
    let store = shaped();
    let mut session = inside(&store);
    session
        .atomically(|work| {
            work.run_with("SET order_rows:1 = { a: 1 };", &Parameters::new())?;
            let mut bound = Parameters::new();
            bound.insert("value".to_owned(), Value::from("two"));
            work.run_with("SET order_rows:2 = { a: $value };", &bound)?;
            Ok(())
        })
        .unwrap();
    let count =
        |session: &mut Session<'_>| match session.run("SELECT * FROM order_rows;").unwrap().pop() {
            Some(Outcome::Records { records, .. }) => records.len(),
            other => panic!("not records: {other:?}"),
        };
    assert_eq!(count(&mut session), 2);

    let failed: tessari_session::Result<()> = session.atomically(|work| {
        work.run_with("SET order_rows:3 = { a: 3 };", &Parameters::new())?;
        work.run_with("SET order_rows:4 = ;", &Parameters::new())?;
        Ok(())
    });
    assert!(failed.is_err());
    assert_eq!(
        count(&mut session),
        2,
        "the first script's write must not be kept"
    );

    let verb = session.atomically(|work| work.run_with("COMMIT;", &Parameters::new()));
    assert!(
        matches!(verb, Err(Error::TransactionVerbInAtomic { .. })),
        "{verb:?}"
    );
}
