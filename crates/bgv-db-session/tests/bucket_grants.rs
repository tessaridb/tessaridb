//! Who may reach a file, and who may reach its bytes.
//!
//! ADR-0011 §6 says a file's metadata and its bytes are behind **one**
//! permission question, because they are one table to grant on. That is a claim
//! about code that was not written for it — `PUT` and `READ` were added to the
//! grant ratchet and nothing else changed — so it is worth asserting from
//! outside rather than reasoning about from inside.
//!
//! The failure this is looking for is the one this project has met twice
//! already, most recently on the change feed: **a read that is not a statement**
//! reaches records without passing the check every other read passes. Reading a
//! file is two reads — the metadata and then the chunks — and the chunks live in
//! a table the grant does not name.

#![allow(clippy::panic, clippy::unwrap_used, clippy::indexing_slicing)]

use std::sync::Arc;

use bgv_db_kv::{KvBackend, MemoryBackend};
use bgv_db_session::{Outcome, Session};
use bgv_db_storage::Store;
use bgv_db_types::Value;

const PASSWORD: &str = "correct horse battery";

fn store() -> Store {
    let backend = Arc::new(MemoryBackend::new()) as Arc<dyn KvBackend>;
    Store::open(backend).unwrap()
}

/// A bucket with a file in it, a table beside it, and two users.
fn ready(store: &Store) {
    let mut session = Session::new(store);
    session
        .run(
            "DEFINE NAMESPACE prod; USE NAMESPACE prod;\n\
             DEFINE DATABASE library; USE DATABASE library;\n\
             DEFINE BUCKET media; DEFINE TABLE users;\n\
             PUT media:'/secret.txt' = 'the salary spreadsheet';\n\
             CREATE users:1 = { name: 'ada' };\n\
             DEFINE USER root ROLE owner PASSWORD 'correct horse battery';",
        )
        .unwrap();

    let mut root = Session::new(store);
    root.sign_in("root", PASSWORD).unwrap();
    root.run(
        "USE NAMESPACE prod; USE DATABASE library;\n\
         DEFINE USER reader ON prod.library ROLE editor PASSWORD 'correct horse battery';\n\
         DEFINE USER outsider ON prod.library ROLE editor PASSWORD 'correct horse battery';\n\
         GRANT read ON media TO reader;\n\
         GRANT read ON users TO outsider;",
    )
    .unwrap();
}

fn signed_in<'a>(store: &'a Store, name: &str) -> Session<'a> {
    let mut session = Session::new(store);
    session.sign_in(name, PASSWORD).unwrap();
    session
        .run("USE NAMESPACE prod; USE DATABASE library;")
        .unwrap();
    session
}

#[test]
fn a_grant_on_the_bucket_reaches_the_bytes() {
    let store = store();
    ready(&store);
    let mut reader = signed_in(&store, "reader");

    let outcomes = reader.run("READ media:'/secret.txt';").unwrap();
    let Outcome::Value(Value::Bytes(bytes)) = &outcomes[0] else {
        panic!("not bytes: {:?}", outcomes[0]);
    };
    assert_eq!(bytes.as_slice(), b"the salary spreadsheet");
}

#[test]
fn a_session_without_the_grant_reaches_neither_half() {
    // Both halves, and separately: the metadata is an ordinary record and would
    // be refused by machinery that already existed, while the bytes live in a
    // table the grant never names. A test that only asserted the first would
    // pass while the second leaked.
    let store = store();
    ready(&store);
    let mut outsider = signed_in(&store, "outsider");

    let metadata = outsider
        .run("SELECT * FROM media;")
        .expect_err("an ungranted session listed a bucket");
    assert!(metadata.to_string().contains("media"), "{metadata}");

    let bytes = outsider
        .run("READ media:'/secret.txt';")
        .expect_err("an ungranted session read a file");
    assert!(bytes.to_string().contains("media"), "{bytes}");
}

#[test]
fn a_read_grant_does_not_carry_a_write() {
    let store = store();
    ready(&store);
    let mut reader = signed_in(&store, "reader");

    let refused = reader
        .run("PUT media:'/secret.txt' = 'overwritten';")
        .expect_err("a read grant wrote a file");
    assert!(refused.to_string().contains("media"), "{refused}");

    // And the file is what it was.
    let outcomes = reader.run("READ media:'/secret.txt';").unwrap();
    let Outcome::Value(Value::Bytes(bytes)) = &outcomes[0] else {
        panic!("not bytes: {:?}", outcomes[0]);
    };
    assert_eq!(bytes.as_slice(), b"the salary spreadsheet");
}

#[test]
fn deleting_a_file_needs_the_grant_that_writing_one_needs() {
    // `DELETE` removes the bytes as well as the metadata, so a read grant
    // reaching it would be the bluntest possible version of this leak.
    let store = store();
    ready(&store);
    let mut reader = signed_in(&store, "reader");

    let refused = reader
        .run("DELETE media:'/secret.txt';")
        .expect_err("a read grant deleted a file");
    assert!(refused.to_string().contains("media"), "{refused}");
}

#[test]
fn a_field_grant_hides_metadata_and_says_nothing_about_the_bytes() {
    // A per-field grant edits the record it answers with. A file's bytes are not
    // a field of that record, so hiding `size` hides `size` — and `READ` is
    // governed by the table grant, which is the one question ADR-0011 §6 says
    // there should be. Asserted so that the two mechanisms cannot be assumed to
    // interact when they do not.
    let store = store();
    ready(&store);
    let mut root = Session::new(&store);
    root.sign_in("root", PASSWORD).unwrap();
    root.run(
        "USE NAMESPACE prod; USE DATABASE library; GRANT read ON media FIELDS chunks TO reader;",
    )
    .unwrap();

    let mut reader = signed_in(&store, "reader");
    let outcomes = reader.run("SELECT * FROM media;").unwrap();
    let Outcome::Records { records, .. } = &outcomes[0] else {
        panic!("not records: {:?}", outcomes[0]);
    };
    let Value::Object(held) = &records[0].1 else {
        panic!("not an object");
    };
    assert!(held.contains_key("chunks"), "the granted field is hidden");
    assert!(!held.contains_key("size"), "an ungranted field is visible");

    let outcomes = reader.run("READ media:'/secret.txt';").unwrap();
    let Outcome::Value(Value::Bytes(bytes)) = &outcomes[0] else {
        panic!("not bytes: {:?}", outcomes[0]);
    };
    assert_eq!(bytes.as_slice(), b"the salary spreadsheet");
}
