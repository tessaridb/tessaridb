//! The same script, one store in this process and one across a socket, and the
//! same characters out of both.
//!
//! # Why this test is the wave rather than a check on it
//!
//! A remote console is only worth having if what it prints is what the embedded
//! one prints. The alternative — a client reading the HTTP endpoint's JSON —
//! would have to *decide* whether `"12.34"` is a decimal and `"2s"` a duration,
//! and would be wrong in a way nobody notices until a value is pasted back into
//! a statement and means something else.
//!
//! Two stores rather than one, seeded identically, so a script that writes can
//! be compared too: the mutation happens once in each, and the answers should
//! still match character for character.
//!
//! # What is compared
//!
//! Every answer shape a statement can produce, which is the correspondence
//! obligation of this wave. `Unknown` is absent because no shipped statement
//! produces it — the enum is `#[non_exhaustive]` so that a *future* store can.

#![allow(clippy::panic, clippy::unwrap_used)]

use std::io::Cursor;
use std::sync::Arc;

use tessari_wire::Node;
use tessaridb::{Db, Parameters, Value};

use crate::session::{Mode, Piped, run};
use crate::store::{Embedded, Remote, Store};

// A binary has no library target to depend on, and giving it one to make a test
// possible would be shaping the crate around its test. Included the way
// `roundtrip.rs` already includes the renderer.
#[path = "../src/render.rs"]
mod render;
#[path = "../src/session.rs"]
mod session;
#[path = "../src/store.rs"]
mod store;
#[path = "../src/table.rs"]
mod table;

/// Enough of a store to answer every shape, written the same way twice.
const SEED: &str = "\
DEFINE NAMESPACE prod; USE NAMESPACE prod; \
DEFINE DATABASE orders; USE DATABASE orders; \
DEFINE TABLE users; DEFINE TABLE readings; \
DEFINE TABLE sessions; DEFINE TABLE k; \
CREATE users:1 = { name: 'ada', rank: 1 }; \
CREATE users:2 = { name: 'grace', rank: 2 }; \
CREATE readings:1 = { at: datetime '2026-01-01T00:00:00Z', value: 1 }; \
CREATE readings:2 = { at: datetime '2026-01-02T00:00:00Z', value: 2 }; \
SET sessions:'abc' = { user: users:1, span: 2s, exact: dec 12.34, \
                       raw: 0xdeadbeef, when: datetime '2026-03-04T05:06:07Z', \
                       absent: NONE, empty: NULL, listed: [1, 2.5, 'three'] }; \
SET sessions:'def' = 7;";

/// Where the session goes, once the store is seeded.
const SELECTED: &str = "USE NAMESPACE prod; USE DATABASE orders;";

/// The scripts whose output must match, one per answer shape.
fn scripts() -> Vec<(&'static str, &'static str)> {
    vec![
        ("records via a scan", "SELECT * FROM users;"),
        ("records via an identity", "SELECT * FROM users:1;"),
        ("no records at all", "SELECT * FROM users WHERE rank > 99;"),
        // The whole argument for carrying the store's codec: fifteen types out,
        // fifteen back, and a decimal that is still a decimal.
        ("a value of every kind", "GET sessions:'abc';"),
        ("a value that is a reference", "SET k:1 = users:2; GET k:1;"),
        ("a plain value", "GET sessions:'def';"),
        ("keys", "KEYS FROM sessions;"),
        ("a statement that only does work", "DEFINE TABLE later;"),
        (
            "a conditional delete's count",
            "DELETE FROM readings WHERE value >= 1;",
        ),
    ]
}

/// Run a script against a store in this process, and collect what was printed.
fn embedded(db: &Db, script: &str) -> String {
    let mut store = Embedded::new(db, None, Parameters::new()).expect("a session");
    said(&mut store, script)
}

/// The same, against a node.
fn remote(address: &str, script: &str) -> String {
    let mut store = Remote::connect(address, None, Parameters::new()).expect("a connection");
    said(&mut store, script)
}

fn said(store: &mut dyn Store, script: &str) -> String {
    let mut input = Piped::new(Cursor::new(format!("{SELECTED} {script}").into_bytes()));
    let mut out = Vec::new();
    run(store, &mut input, &mut out, Mode::Script).expect("a run");
    String::from_utf8(out).expect("text")
}

#[test]
fn every_answer_shape_reads_the_same_from_a_socket_as_from_this_process() {
    let here = Db::in_memory().unwrap();
    let there = Arc::new(Db::in_memory().unwrap());
    for db in [&here, there.as_ref()] {
        let mut store = Embedded::new(db, None, Parameters::new()).expect("a session");
        store.run(SEED).expect("a seeded store");
    }

    let node = Arc::new(Node::bind(Arc::clone(&there), "127.0.0.1:0").unwrap());
    let address = node.address().unwrap();
    let serving = Arc::clone(&node);
    drop(std::thread::spawn(move || serving.serve()));

    for (what, script) in scripts() {
        let near = embedded(&here, script);
        let far = remote(&address, script);
        assert_eq!(
            near, far,
            "{what} differs\nembedded:\n{near}\nremote:\n{far}"
        );
        assert!(!near.contains("error:"), "{what} was refused:\n{near}");
    }
}

#[test]
fn a_parameterised_script_reads_the_same_from_a_socket_as_from_this_process() {
    // The parity property, extended to the values a caller supplies. Both paths
    // hold their own bindings and neither is allowed to reach a different
    // answer — which is the half a `--param` flag could silently get wrong, by
    // binding on one side and interpolating on the other.
    let here = Db::in_memory().unwrap();
    let there = Arc::new(Db::in_memory().unwrap());
    for db in [&here, there.as_ref()] {
        let mut store = Embedded::new(db, None, Parameters::new()).expect("a session");
        store.run(SEED).expect("a seeded store");
    }

    let node = Arc::new(Node::bind(Arc::clone(&there), "127.0.0.1:0").unwrap());
    let address = node.address().unwrap();
    let serving = Arc::clone(&node);
    drop(std::thread::spawn(move || serving.serve()));

    let mut given = Parameters::new();
    given.insert("who".to_owned(), Value::String("ada".to_owned()));
    let script = "SELECT * FROM users WHERE name = $who;";

    let mut near_store = Embedded::new(&here, None, given.clone()).expect("a session");
    let near = said(&mut near_store, script);
    let mut far_store = Remote::connect(&address, None, given).expect("a connection");
    let far = said(&mut far_store, script);

    assert_eq!(near, far, "embedded:\n{near}\nremote:\n{far}");
    assert!(!near.contains("error:"), "refused:\n{near}");
    assert!(near.contains("ada"), "the parameter did not bind:\n{near}");
}

#[test]
fn a_reference_renders_as_a_name_over_the_wire_and_not_as_an_id() {
    // Without the names the answer carries, this reads `<record 3:2>` — which is
    // exactly what the renderer exists to avoid, since what is printed is
    // supposed to paste back into the next statement.
    let db = Arc::new(Db::in_memory().unwrap());
    {
        let mut store = Embedded::new(&db, None, Parameters::new()).expect("a session");
        store.run(SEED).expect("a seeded store");
    }
    let node = Arc::new(Node::bind(Arc::clone(&db), "127.0.0.1:0").unwrap());
    let address = node.address().unwrap();
    let serving = Arc::clone(&node);
    drop(std::thread::spawn(move || serving.serve()));

    let said = remote(&address, "GET sessions:'abc';");
    assert!(said.contains("user: users:1"), "{said}");
    assert!(!said.contains("<record"), "{said}");
}

#[test]
fn a_refusal_from_a_node_reads_like_a_refusal_from_a_store() {
    let here = Db::in_memory().unwrap();
    let there = Arc::new(Db::in_memory().unwrap());
    let node = Arc::new(Node::bind(Arc::clone(&there), "127.0.0.1:0").unwrap());
    let address = node.address().unwrap();
    let serving = Arc::clone(&node);
    drop(std::thread::spawn(move || serving.serve()));

    // Not identical text — the store's message travels, the socket's framing
    // does not — but both must be reported as a refusal rather than as silence.
    let near = embedded(&here, "SELECT * FROM;");
    let far = remote(&address, "SELECT * FROM;");
    assert!(near.contains("error:"), "{near}");
    assert!(far.contains("error:"), "{far}");
    assert!(far.contains("expected"), "{far}");
}
