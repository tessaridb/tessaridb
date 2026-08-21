//! A client and a node, over a real socket.
//!
//! These are the tests the unit ones cannot be: that a value survives the
//! network as the value it was, that a hostile frame closes one connection and
//! not the node, and that the store's own refusals arrive as refusals rather
//! than as silence.

#![allow(clippy::panic, clippy::unwrap_used, clippy::indexing_slicing)]

use std::io::Write;
use std::net::TcpStream;
use std::sync::Arc;

use bgv_db::{Db, Value};
use bgv_db_wire::{Answer, Client, Node};

/// A node on a loopback port the operating system picked, plus its address.
fn serving(db: Db) -> (Arc<Node>, String) {
    let node = Arc::new(Node::bind(Arc::new(db), "127.0.0.1:0").unwrap());
    let address = node.address().unwrap();
    let held = Arc::clone(&node);
    drop(std::thread::spawn(move || held.serve()));
    (node, address)
}

const READY: &str = "DEFINE NAMESPACE prod; USE NAMESPACE prod; \
                     DEFINE DATABASE orders; USE DATABASE orders;";

#[test]
fn a_script_runs_and_answers_one_outcome_per_statement() {
    let (_node, address) = serving(Db::in_memory().unwrap());
    let mut client = Client::connect(&address).unwrap();

    let answers = client
        .run(
            &format!("{READY} DEFINE TABLE users; CREATE users:1 = {{ name: 'ada' }}; SELECT * FROM users;"),
            None,
        )
        .unwrap();
    assert_eq!(answers.len(), 7);
    match &answers[6] {
        Answer::Records { records, path } => {
            assert_eq!(records.len(), 1);
            assert_eq!(records[0].0, "1");
            assert_eq!(path, "scan");
        }
        other => panic!("not records: {other:?}"),
    }
}

#[test]
fn a_value_crosses_the_network_as_the_value_it_was() {
    // The whole reason this protocol exists rather than the console reading the
    // HTTP endpoint. Every kind that JSON would have flattened into a string
    // comes back as itself, so a client decides nothing.
    let (_node, address) = serving(Db::in_memory().unwrap());
    let mut client = Client::connect(&address).unwrap();
    client
        .run(&format!("{READY} DEFINE TABLE probe;"), None)
        .unwrap();
    client
        .run(
            "USE NAMESPACE prod; USE DATABASE orders; \
             CREATE probe:1 = { exact: dec 12.34, real: 1.5, whole: 42, span: 1h30m, \
             at: datetime '2026-01-15T09:30:00Z', raw: 0x0a1b, \
             who: uuid '00112233-4455-6677-8899-aabbccddeeff', \
             list: [1, 'two'], nested: { inner: 1 }, absent: NONE, empty: NULL };",
            None,
        )
        .unwrap();

    let answers = client
        .run(
            "USE NAMESPACE prod; USE DATABASE orders; SELECT * FROM probe:1;",
            None,
        )
        .unwrap();
    let Answer::Records { records, .. } = &answers[2] else {
        panic!("not records: {:?}", answers[2]);
    };
    let Value::Object(held) = &records[0].1 else {
        panic!("not an object");
    };

    // A decimal is a decimal, not a quoted string a client has to decide about.
    assert!(matches!(
        held.get("exact"),
        Some(Value::Number(bgv_db::Number::Decimal(_)))
    ));
    assert!(matches!(
        held.get("real"),
        Some(Value::Number(bgv_db::Number::Float(_)))
    ));
    assert!(matches!(
        held.get("whole"),
        Some(Value::Number(bgv_db::Number::Integer(42)))
    ));
    assert!(matches!(held.get("span"), Some(Value::Duration(_))));
    assert!(matches!(held.get("at"), Some(Value::Datetime(_))));
    assert!(matches!(held.get("raw"), Some(Value::Bytes(_))));
    assert!(matches!(held.get("who"), Some(Value::Uuid(_))));
    assert!(matches!(held.get("list"), Some(Value::Array(_))));
    assert!(matches!(held.get("nested"), Some(Value::Object(_))));
    // `none` and `null` are two values and arrive as two values. JSON has one
    // word for both, so the HTTP surface carries the distinction by whether it
    // writes the key at all; here there is nothing to carry it with, because
    // nothing was lost.
    assert_eq!(held.get("absent"), Some(&Value::None));
    assert_eq!(held.get("empty"), Some(&Value::Null));
}

#[test]
fn a_refusal_arrives_as_a_refusal_and_the_connection_lives() {
    // A client that mistyped a statement has not stopped being a client.
    let (_node, address) = serving(Db::in_memory().unwrap());
    let mut client = Client::connect(&address).unwrap();

    let refused = client.run("SELECT * FROM;", None).expect_err("a refusal");
    let message = refused.to_string();
    assert!(message.contains("expected"), "{message}");

    // And the next statement runs on the same connection.
    let answers = client.run(READY, None).unwrap();
    assert_eq!(answers.len(), 4);
}

#[test]
fn a_closed_store_refuses_an_unauthenticated_request_with_the_sessions_own_words() {
    // The check is the session's. A second rule here would be a second place for
    // "who may do this" to be decided, and the two would eventually disagree.
    let db = Db::in_memory().unwrap();
    {
        let mut session = db.session();
        session
            .run("DEFINE USER root ROLE owner PASSWORD 'a long one';")
            .unwrap();
    }
    let (_node, address) = serving(db);
    let mut client = Client::connect(&address).unwrap();

    let refused = client
        .run("DEFINE NAMESPACE prod;", None)
        .expect_err("a refusal");
    assert!(refused.to_string().contains("signed-in"), "{refused}");

    // With credentials it runs.
    let answers = client
        .run("DEFINE NAMESPACE prod;", Some(("root", "a long one")))
        .unwrap();
    assert_eq!(answers.len(), 1);

    // And a wrong password is the session's refusal, not a different one.
    let refused = client
        .run("DEFINE NAMESPACE other;", Some(("root", "wrong")))
        .expect_err("a refusal");
    assert!(refused.to_string().contains("no user"), "{refused}");
}

#[test]
fn a_frame_claiming_more_than_the_node_will_read_does_not_take_the_node_down() {
    // The oldest denial of service there is: a five-byte header claiming a
    // gibibyte. It is refused before the allocation, the connection closes, and
    // the node keeps serving — asserted by a second client arriving afterwards.
    let (_node, address) = serving(Db::in_memory().unwrap());
    {
        let mut hostile = TcpStream::connect(&address).unwrap();
        // The greeting, so the node gets as far as reading a frame.
        hostile.write_all(b"BGVW").unwrap();
        hostile.write_all(&[1]).unwrap();
        hostile.write_all(&[1]).unwrap();
        hostile.write_all(&1_000_000_000_u32.to_be_bytes()).unwrap();
        hostile.flush().unwrap();
    }

    let mut client = Client::connect(&address).unwrap();
    assert_eq!(client.run(READY, None).unwrap().len(), 4);
}

#[test]
fn rubbish_on_the_socket_is_refused_at_the_greeting() {
    let (_node, address) = serving(Db::in_memory().unwrap());
    {
        let mut hostile = TcpStream::connect(&address).unwrap();
        hostile.write_all(b"GET / HTTP/1.1\r\n\r\n").unwrap();
        hostile.flush().unwrap();
    }
    // The node survives it.
    let mut client = Client::connect(&address).unwrap();
    assert_eq!(client.run(READY, None).unwrap().len(), 4);
}

#[test]
fn talking_to_something_that_is_not_a_node_says_so_rather_than_hanging() {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap().to_string();
    drop(std::thread::spawn(move || {
        if let Ok((mut stream, _)) = listener.accept() {
            drop(stream.write_all(b"HTTP/1.1 200 OK\r\n\r\n"));
        }
    }));

    let refused = Client::connect(&address).expect_err("a refusal");
    assert!(
        refused.to_string().contains("not a bgv-db node"),
        "{refused}"
    );
}

#[test]
fn two_clients_at_once_do_not_interfere() {
    let (_node, address) = serving(Db::in_memory().unwrap());
    {
        let mut setup = Client::connect(&address).unwrap();
        setup
            .run(&format!("{READY} DEFINE TABLE counters;"), None)
            .unwrap();
    }

    let handles: Vec<_> = (0..4)
        .map(|worker| {
            let address = address.clone();
            std::thread::spawn(move || {
                let mut client = Client::connect(&address).unwrap();
                for n in 0..10 {
                    let id = worker * 100 + n;
                    client
                        .run(
                            &format!(
                                "USE NAMESPACE prod; USE DATABASE orders; \
                                 CREATE counters:{id} = {{ worker: {worker} }};"
                            ),
                            None,
                        )
                        .unwrap();
                }
            })
        })
        .collect();
    for handle in handles {
        handle.join().unwrap();
    }

    let mut client = Client::connect(&address).unwrap();
    let answers = client
        .run(
            "USE NAMESPACE prod; USE DATABASE orders; \
             SELECT count(*) AS held FROM counters;",
            None,
        )
        .unwrap();
    let Answer::Records { records, .. } = &answers[2] else {
        panic!("not records");
    };
    let Value::Object(held) = &records[0].1 else {
        panic!("not an object");
    };
    assert_eq!(
        held.get("held"),
        Some(&Value::Number(bgv_db::Number::from(40_i64)))
    );
}
