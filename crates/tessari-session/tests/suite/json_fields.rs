//! The declared-type exception: JSON text written to an object-declared field
//! reads as the object it spells.
//!
//! The store refuses rather than coerces by discipline — a comparison narrows,
//! arithmetic refuses, a wrong-typed write fails the declaration it was checked
//! against. An object field receiving JSON text is the one place the
//! declaration itself asks for the parse: the wire's json/jsonb columns declare
//! `object`, and every client of the dialect speaks JSON text. So the parse is the
//! declared kind's own reading of the value, taken deliberately, and a text
//! that does not read refuses naming the field and the byte — never stored as
//! a string the declaration did not accept, never silently dropped.
//!
//! # What this is not
//!
//! A general coercion. A string field still keeps its text, a number field
//! still refuses a string, and the exception fires only where the declaration
//! says `object` and the value is text — the one pair where the value's own
//! spelling is the type's interchange format.

#![allow(clippy::panic, clippy::unwrap_used)]

use std::sync::Arc;
use tessari_session::Now;
use tessari_session::RunNow;

use tessari_kv::{KvBackend, MemoryBackend};
use tessari_session::{Error, Outcome, Session};
use tessari_storage::Store;
use tessari_types::Value;

fn store() -> Store {
    Store::open(Arc::new(MemoryBackend::new()) as Arc<dyn KvBackend>)
        .now()
        .unwrap()
}

/// One table whose field declares the object kind.
const SCHEMA: &str = "\
DEFINE NAMESPACE prod; USE NAMESPACE prod;
DEFINE DATABASE shop; USE DATABASE shop;
DEFINE TABLE docs SCHEMALESS;
DEFINE FIELD meta ON docs TYPE object;
";

fn ready(store: &Store) -> Session<'_> {
    let mut session = Session::new(store);
    session.run_now(SCHEMA).expect("the schema defines");
    session
}

#[test]
fn json_text_written_to_an_object_field_reads_as_the_object() {
    let store = store();
    let mut session = ready(&store);
    session
        .run_now("CREATE docs:1 = { meta: '{\"a\": 1, \"b\": {\"c\": true}}' }")
        .expect("the JSON text parses into the declared kind");
    let outcome = session
        .run_now("SELECT meta.a, meta.b.c FROM docs:1")
        .expect("the read answers");
    let Some(Outcome::Records { records, .. }) = outcome.into_iter().last() else {
        panic!("one statement answers records");
    };
    assert!(
        matches!(&records[0].1, Value::Object(fields)
            if fields.get("a") == Some(&Value::Number(1_i64.into()))
                && fields.get("c") == Some(&Value::Bool(true))),
        "the object arrived, addressable by path: {records:?}"
    );
    // The field itself holds the object — the wire renders it as JSON text.
    let outcome = session
        .run_now("SELECT meta FROM docs:1")
        .expect("the read answers");
    let Some(Outcome::Records { records, .. }) = outcome.into_iter().last() else {
        panic!("one statement answers records");
    };
    assert!(
        matches!(&records[0].1, Value::Object(record)
            if matches!(record.get("meta"), Some(Value::Object(fields))
                if fields.contains_key("a") && fields.contains_key("b"))),
        "the field holds the object itself: {records:?}"
    );
}

#[test]
fn malformed_json_text_refuses_naming_the_field() {
    let store = store();
    let mut session = ready(&store);
    let refused = session
        .run_now("CREATE docs:1 = { meta: '{unclosed' }")
        .expect_err("a text that does not parse refuses");
    let Error::JsonTextRefused { field, .. } = refused else {
        panic!("the parse refusal names the field: {refused:?}");
    };
    assert_eq!(field, "meta", "the field the text was written to");
    // The refusal left nothing behind.
    let outcome = session
        .run_now("SELECT id FROM docs")
        .expect("the listing answers");
    let Some(Outcome::Records { records, .. }) = outcome.into_iter().last() else {
        panic!("one statement answers records");
    };
    assert!(
        records.is_empty(),
        "the refused write stored nothing: {records:?}"
    );
}

#[test]
fn an_update_writes_json_text_the_same_way() {
    let store = store();
    let mut session = ready(&store);
    session
        .run_now("CREATE docs:1 = { meta: '{\"a\": 1}' }")
        .expect("the create parses");
    session
        .run_now("UPDATE docs:1 SET meta = '{\"a\": 2, \"d\": null}'")
        .expect("the update parses the text the same way");
    let outcome = session
        .run_now("SELECT meta.a, meta.d FROM docs:1")
        .expect("the read answers");
    let Some(Outcome::Records { records, .. }) = outcome.into_iter().last() else {
        panic!("one statement answers records");
    };
    assert!(
        matches!(&records[0].1, Value::Object(fields)
            if fields.get("a") == Some(&Value::Number(2_i64.into()))
                && fields.contains_key("d")),
        "the update's text became the object: {records:?}"
    );
}

#[test]
fn other_kinds_keep_their_discipline() {
    let store = store();
    let mut session = Session::new(&store);
    session
        .run_now(
            "DEFINE NAMESPACE prod; USE NAMESPACE prod;\n\
             DEFINE DATABASE shop; USE DATABASE shop;\n\
             DEFINE TABLE notes SCHEMALESS;\n\
             DEFINE FIELD body ON notes TYPE string;\n\
             DEFINE FIELD band ON notes TYPE int;",
        )
        .expect("the schema defines");
    // A string field keeps its text — JSON-looking or not.
    session
        .run_now("CREATE notes:1 = { body: '{\"not\": \"parsed\"}', band: 3 }")
        .expect("the string field keeps the text");
    // A number field still refuses a string.
    let refused = session
        .run_now("UPDATE notes:1 SET band = '{\"not\": \"parsed\"}'")
        .expect_err("a string is not an int");
    assert!(
        matches!(refused, Error::Store(_)),
        "the kind discipline is untouched: {refused:?}"
    );
}

#[test]
fn the_generated_identity_create_coerces_the_same_way() {
    let store = store();
    let mut session = ready(&store);
    session
        .run_now("CREATE docs = { meta: '{\"a\": 9}' } RETURN AFTER")
        .expect("the store-named create parses the text too");
    let outcome = session
        .run_now("SELECT meta.a FROM docs")
        .expect("the read answers");
    let Some(Outcome::Records { records, .. }) = outcome.into_iter().last() else {
        panic!("one statement answers records");
    };
    assert!(
        matches!(&records[0].1, Value::Object(record)
            if matches!(record.get("a"), Some(Value::Number(n)) if n.to_string() == "9")),
        "the generated create took the coercion: {records:?}"
    );
}
