//! A route step naming a key that is not a name: `headers['content-type']`.
//!
//! A record may hold `{ 'content-type': 'text/plain' }` — an object key is any
//! text — but a route step was only ever a name or a position, so the field
//! could be written and never read back except by taking the whole object. The
//! bracketed string is the step for every key a name cannot spell.

#![allow(clippy::panic, clippy::unwrap_used)]

use std::sync::Arc;
use tessari_session::Now;
use tessari_session::RunNow;

use tessari_kv::{KvBackend, MemoryBackend};
use tessari_session::Session;
use tessari_storage::Store;

fn store() -> Store {
    Store::open(Arc::new(MemoryBackend::new()) as Arc<dyn KvBackend>)
        .now()
        .unwrap()
}

/// A collection holding two objects whose header keys are not names.
fn ready(store: &Store) -> Session<'_> {
    let mut session = Session::new(store);
    session
        .run_now(
            "DEFINE NAMESPACE prod; USE NAMESPACE prod;\n\
             DEFINE DATABASE files; USE DATABASE files;\n\
             DEFINE COLLECTION objects;\n\
             CREATE objects:1 = { headers: { 'content-type': 'text/plain', 'x-amz-meta': 'a' } };\n\
             CREATE objects:2 = { headers: { 'content-type': 'image/png' } };",
        )
        .unwrap();
    session
}

fn last(session: &mut Session<'_>, script: &str) -> String {
    let answered = session.run_now(script).unwrap();
    format!("{:?}", answered.last().unwrap())
}

#[test]
fn a_bracketed_key_reads_a_field_a_name_cannot_spell() {
    let store = store();
    let mut session = ready(&store);
    let answer = last(
        &mut session,
        "SELECT headers['content-type'] AS kind FROM objects:1;",
    );
    assert!(
        answer.contains("text/plain"),
        "the key's value, got {answer}"
    );
}

#[test]
fn a_bracketed_key_sets_a_field_and_keeps_its_neighbours() {
    let store = store();
    let mut session = ready(&store);
    session
        .run_now("UPDATE objects:1 SET headers['content-type'] = 'application/json';")
        .unwrap();
    let answer = last(&mut session, "SELECT headers FROM objects:1;");
    assert!(
        answer.contains("application/json") && !answer.contains("text/plain"),
        "the key holds the new value, got {answer}"
    );
    assert!(
        answer.contains("x-amz-meta"),
        "a set changes one key and keeps the rest, got {answer}"
    );
}

#[test]
fn a_bracketed_key_filters_a_read() {
    let store = store();
    let mut session = ready(&store);
    let answer = last(
        &mut session,
        "SELECT id FROM objects WHERE headers['content-type'] = 'image/png';",
    );
    assert!(answer.contains('2'), "record 2 matches, got {answer}");
    assert!(!answer.contains('1'), "record 1 does not, got {answer}");
}

#[test]
fn an_index_on_a_bracketed_key_serves_the_read() {
    let store = store();
    let mut session = ready(&store);
    session
        .run_now("DEFINE INDEX by_kind ON objects FIELDS headers['content-type'];")
        .unwrap();
    // The route is stored in the catalog as text and read back from it, so the
    // index answering proves the step survived that round trip.
    let answer = last(
        &mut session,
        "SELECT id FROM objects WHERE headers['content-type'] = 'text/plain' USING INDEX by_kind;",
    );
    assert!(
        answer.contains('1'),
        "record 1 through the index, got {answer}"
    );
    assert!(!answer.contains('2'), "record 2 is not text, got {answer}");
}

#[test]
fn a_bracketed_key_follows_a_value_as_well_as_a_name() {
    let store = store();
    let mut session = ready(&store);
    session
        .run_now(
            "DEFINE COLLECTION seen;\n\
             DEFINE EVENT kinds ON objects FOR UPDATE THEN\n\
                 CREATE seen = { kind: $after.headers['content-type'] };\n\
             UPDATE objects:2 SET size = 10;",
        )
        .unwrap();
    let answer = last(&mut session, "SELECT kind FROM seen;");
    assert!(
        answer.contains("image/png"),
        "the event read the key, got {answer}"
    );
}

#[test]
fn a_key_holding_a_route_delimiter_is_refused_where_it_is_written() {
    let store = store();
    let mut session = ready(&store);
    let refusal = session
        .run_now("SELECT headers['a.b'] AS x FROM objects;")
        .unwrap_err()
        .to_string();
    assert!(
        refusal.contains("a key without `.`, `[` or `]`"),
        "a stored route could not hold it, got {refusal}"
    );
}
