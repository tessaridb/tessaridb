//! The split: a table declares its fields, a collection does not.
//!
//! # What the pair of tests is for
//!
//! Either half alone is satisfiable by the wrong implementation. A collection
//! that accepts an undeclared field proves nothing if a table accepts one too —
//! that is the behaviour before this node, and every test of it passed. A table
//! that refuses one proves nothing if a collection refuses it as well, because
//! then the word bought a synonym. So each record is written to **both**, and
//! the assertion is that they answer differently.
//!
//! # Why the refusal's text is asserted and not merely its existence
//!
//! `DEFINE TABLE t;` is the shortest declaration that worked before this node
//! and it now fails. A refusal that does not name what to write instead turns a
//! one-word fix into a search through the specification, so the message names
//! both replacements — the collection, and the explicit lenient table an older
//! script wants — and that is a promise worth a test.

#![allow(clippy::panic, clippy::unwrap_used)]

use std::sync::Arc;

use tessari_kv::{KvBackend, MemoryBackend};
use tessari_session::Session;
use tessari_storage::Store;

fn store() -> Store {
    Store::open(Arc::new(MemoryBackend::new()) as Arc<dyn KvBackend>).unwrap()
}

fn ready(store: &Store) -> Session<'_> {
    let mut session = Session::new(store);
    session
        .run("DEFINE NAMESPACE prod; USE NAMESPACE prod; DEFINE DATABASE shop; USE DATABASE shop;")
        .unwrap();
    session
}

#[test]
fn a_collection_takes_a_field_nobody_declared_and_a_table_refuses_it() {
    let store = store();
    let mut session = ready(&store);

    session.run("DEFINE COLLECTION notes;").unwrap();
    session.run("DEFINE TABLE people (name string);").unwrap();

    // The same record, to both. Anything else compares two writes rather than
    // two declarations.
    let record = "= { name: 'ada', nickname: 'the countess' }";

    session
        .run(&format!("CREATE notes:1 {record};"))
        .unwrap_or_else(|error| panic!("a collection refused an undeclared field: {error}"));

    let refused = session.run(&format!("CREATE people:1 {record};"));
    assert!(
        refused.is_err(),
        "a table accepted a field it does not declare, so the word bought nothing"
    );
}

#[test]
fn a_declared_table_is_strict_without_being_told_to_be() {
    let store = store();
    let mut session = ready(&store);

    // No `SCHEMAFULL`. That is the whole assertion: the default moved.
    session.run("DEFINE TABLE people (name string);").unwrap();

    let refused = session.run("CREATE people:1 = { name: 'ada', nickname: 'x' };");
    assert!(
        refused.is_err(),
        "a declared table accepted an undeclared field by default"
    );
}

#[test]
fn schemaless_is_the_way_back_and_still_works() {
    let store = store();
    let mut session = ready(&store);

    session
        .run("DEFINE TABLE people (name string) SCHEMALESS;")
        .unwrap();

    session
        .run("CREATE people:1 = { name: 'ada', nickname: 'the countess' };")
        .unwrap_or_else(|error| panic!("SCHEMALESS did not restore the lenient reading: {error}"));
}

#[test]
fn a_table_declaring_nothing_is_refused_and_told_what_to_write() {
    let store = store();
    let mut session = ready(&store);

    let refused = session.run("DEFINE TABLE people;");
    let message = format!(
        "{}",
        refused.expect_err("a table declaring no fields was accepted")
    );

    // Both replacements, because the reader who wrote this wanted one of two
    // different things and the refusal cannot tell which.
    assert!(
        message.contains("DEFINE COLLECTION people"),
        "the refusal does not name the collection: {message}"
    );
    assert!(
        message.contains("SCHEMALESS"),
        "the refusal does not name the way to keep the older reading: {message}"
    );
}

#[test]
fn a_stored_lenient_table_keeps_accepting_what_it_accepted() {
    // G017's kill criterion, inherited whole: strictness is a property of the
    // stored table, so a table declared lenient goes on being lenient no matter
    // what the default becomes afterwards. The declaration and the write are
    // deliberately in different sessions — a single session could satisfy this
    // by carrying the flag in memory rather than reading it back.
    let store = store();
    {
        let mut declaring = ready(&store);
        declaring
            .run("DEFINE TABLE people (name string) SCHEMALESS;")
            .unwrap();
    }

    let mut writing = Session::new(&store);
    writing
        .run("USE NAMESPACE prod; USE DATABASE shop;")
        .unwrap();
    writing
        .run("CREATE people:1 = { name: 'ada', nickname: 'the countess' };")
        .unwrap_or_else(|error| panic!("a stored lenient table stopped being lenient: {error}"));
}

#[test]
fn a_collection_and_a_lenient_table_are_not_the_same_declaration() {
    // They behave alike, which is exactly why the store must keep them apart:
    // `INFO FOR TABLE` has to answer with the word that created the thing, and
    // it cannot if the two were collapsed into one stored shape.
    let store = store();
    let mut session = ready(&store);
    session.run("DEFINE COLLECTION notes;").unwrap();
    session
        .run("DEFINE TABLE people (name string) SCHEMALESS;")
        .unwrap();

    let catalog = format!("{:?}", session.run("INFO FOR DATABASE;").unwrap());
    assert!(
        catalog.contains("notes") && catalog.contains("people"),
        "the catalog does not hold both: {catalog}"
    );
}
