//! A parameter that crosses a socket.
//!
//! The grammar's rule is that a supplied value cannot become syntax, and it
//! holds because binding happens after parsing. Distance is exactly where a rule
//! like that quietly stops holding: if the wire carried a parameter as *text*
//! for the server to parse, the property would be handed back at the last step
//! and nobody would notice until it mattered.
//!
//! So the values here travel in the store's own codec, and these are the tests
//! that say so from outside: every awkward kind arrives as itself, an unbound
//! name is refused with the session's own words, and the adversarial string is
//! still a string when it lands.

#![allow(clippy::panic, clippy::unwrap_used, clippy::indexing_slicing)]

use std::sync::Arc;

use tessari::{Db, Parameters, Value};
use tessari_wire::{Answer, Client, Node};

fn serving(db: Db) -> (Arc<Node>, String) {
    let node = Arc::new(Node::bind(Arc::new(db), "127.0.0.1:0").unwrap());
    let address = node.address().unwrap();
    let held = Arc::clone(&node);
    drop(std::thread::spawn(move || held.serve()));
    (node, address)
}

const READY: &str = "DEFINE NAMESPACE prod; USE NAMESPACE prod; \
                     DEFINE DATABASE orders; USE DATABASE orders; \
                     DEFINE TABLE users; \
                     CREATE users:1 = { name: 'ada', age: 36 }; \
                     CREATE users:2 = { name: 'grace', age: 45 };";

fn one(name: &str, value: Value) -> Parameters {
    let mut parameters = Parameters::new();
    parameters.insert(name.to_owned(), value);
    parameters
}

/// The records the last answer holds.
fn records(answers: &[Answer]) -> Vec<(String, Value)> {
    match answers.last().unwrap() {
        Answer::Records { records, .. } => records.clone(),
        other => panic!("not records: {other:?}"),
    }
}

#[test]
fn a_parameter_binds_across_the_connection() {
    let (_node, address) = serving(Db::in_memory().unwrap());
    let mut client = Client::connect(&address).unwrap();
    client.run(READY, None).unwrap();

    let answers = client
        .run_with(
            "SELECT * FROM users WHERE name = $who;",
            None,
            &one("who", Value::String("grace".to_owned())),
        )
        .unwrap();
    let found = records(&answers);
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].0, "2");
}

#[test]
fn every_awkward_kind_survives_as_a_parameter() {
    // The kinds JSON would have flattened. Each is written through a parameter
    // and read back, so what is asserted is the whole path — encode, frame,
    // decode, bind, store, read — and not the codec on its own, which already
    // has its own tests.
    let (_node, address) = serving(Db::in_memory().unwrap());
    let mut client = Client::connect(&address).unwrap();
    client.run(READY, None).unwrap();

    // The values come from the store rather than from constructors here, which
    // makes this the round trip that actually matters: what a client was
    // *answered* can be sent straight back as a parameter and mean the same
    // thing. A value that could not make that trip would be one a caller has to
    // convert, which is the guessing this protocol exists to remove.
    client
        .run(
            "CREATE users:'probe' = { exact: dec 12.34, span: 1h30m, \
             at: datetime '2026-01-15T09:30:00Z', raw: 0x0a1b, \
             who: uuid '00112233-4455-6677-8899-aabbccddeeff', \
             list: [1, 'two'], nested: { inner: 1 }, empty: NULL };",
            None,
        )
        .unwrap();
    let answers = client.run("SELECT * FROM users:'probe';", None).unwrap();
    let Value::Object(probe) = &records(&answers)[0].1 else {
        panic!("not an object");
    };

    for (name, value) in probe.clone() {
        let mut parameters = Parameters::new();
        parameters.insert("held".to_owned(), value.clone());
        client
            .run_with(
                &format!("CREATE users:'held-{name}' = {{ held: $held }};"),
                None,
                &parameters,
            )
            .unwrap();
        let answers = client
            .run(&format!("SELECT held FROM users:'held-{name}';"), None)
            .unwrap();
        let found = records(&answers);
        let Value::Object(row) = &found[0].1 else {
            panic!("not an object: {:?}", found[0].1);
        };
        // `NONE` is absence, so the projection omits the key rather than
        // writing one — which is the store's own rule and not a loss here.
        let held = row.get("held").cloned().unwrap_or(Value::None);
        assert_eq!(held, value, "{name} did not survive");
    }
}

#[test]
fn a_parameter_holding_a_statement_is_still_a_string_at_this_distance() {
    let (_node, address) = serving(Db::in_memory().unwrap());
    let mut client = Client::connect(&address).unwrap();
    client.run(READY, None).unwrap();

    let attack = "ada'; DELETE FROM users WHERE age > 0; --";
    let answers = client
        .run_with(
            "SELECT * FROM users WHERE name = $who;",
            None,
            &one("who", Value::String(attack.to_owned())),
        )
        .unwrap();
    assert!(records(&answers).is_empty());

    let survivors = client.run("SELECT * FROM users;", None).unwrap();
    assert_eq!(records(&survivors).len(), 2, "the bound text ran");
}

#[test]
fn an_unbound_parameter_is_refused_in_the_sessions_own_words() {
    // The refusal is the session's, carried rather than restated — the same
    // rule the store's other refusals follow, so a remote caller and a local one
    // read the same sentence.
    let (_node, address) = serving(Db::in_memory().unwrap());
    let mut client = Client::connect(&address).unwrap();
    client.run(READY, None).unwrap();

    let refusal = client
        .run("SELECT * FROM users WHERE name = $who;", None)
        .expect_err("an unbound parameter was accepted");
    let said = refusal.to_string();
    assert!(said.contains("who"), "{said}");
}

#[test]
fn a_request_with_no_parameters_is_what_it_always_was() {
    // The protocol gained a field; a script that supplies nothing must answer
    // exactly as before, which is what makes the version bump the only thing an
    // existing caller has to notice.
    let (_node, address) = serving(Db::in_memory().unwrap());
    let mut client = Client::connect(&address).unwrap();
    client.run(READY, None).unwrap();

    let answers = client.run("SELECT * FROM users;", None).unwrap();
    assert_eq!(records(&answers).len(), 2);
}
