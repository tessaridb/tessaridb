//! What an application on the store had to write around (G070, ADR-0124).
//!
//! Each module is one item the S3 server worked around on 0.33.2: the form it
//! had to use is the control, and the form it wanted is the assertion.

mod bound_counts;
mod classes;
mod dedup;
mod drops;
mod event_replace;
mod expiry_and_events;
mod params;
mod release_later;
mod routes;
mod time_arithmetic;

use std::collections::BTreeMap;
use std::sync::Arc;

use tessari_kv::{KvBackend, MemoryBackend};
use tessari_session::{Error, Outcome, Session};
use tessari_storage::Store;
use tessari_types::Value;

/// A store holding `prod`/`app`.
fn store() -> Store {
    let store = Store::open(Arc::new(MemoryBackend::new()) as Arc<dyn KvBackend>).unwrap();
    Session::new(&store)
        .run("DEFINE NAMESPACE prod; USE NAMESPACE prod; DEFINE DATABASE app; USE DATABASE app;")
        .unwrap();
    store
}

/// A session inside `prod`/`app`.
fn inside(store: &Store) -> Session<'_> {
    let mut session = Session::new(store);
    session
        .run("USE NAMESPACE prod; USE DATABASE app;")
        .unwrap();
    session
}

/// The last statement's outcome.
fn run(session: &mut Session<'_>, script: &str) -> Outcome {
    session
        .run(script)
        .unwrap_or_else(|error| panic!("{script}: {error}"))
        .pop()
        .unwrap()
}

/// The refusal a script meets.
fn refused(session: &mut Session<'_>, script: &str) -> Error {
    match session.run(script) {
        Err(error) => error,
        Ok(outcomes) => panic!("{script} was accepted: {outcomes:?}"),
    }
}

/// What a `RETURN` answers.
fn value(session: &mut Session<'_>, script: &str) -> Value {
    match run(session, script) {
        Outcome::Value(value) => value,
        other => panic!("{script} answered {other:?}"),
    }
}

/// The records a read answered, as objects.
fn rows(session: &mut Session<'_>, read: &str) -> Vec<BTreeMap<String, Value>> {
    match run(session, read) {
        Outcome::Records { records, .. } => records
            .into_iter()
            .map(|(_, value)| match value {
                Value::Object(fields) => fields,
                other => panic!("{read}: not an object: {other:?}"),
            })
            .collect(),
        other => panic!("{read} answered {other:?}"),
    }
}
