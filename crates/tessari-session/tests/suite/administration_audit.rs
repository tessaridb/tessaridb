//! A change to who may do what, or to who belongs to the cluster, is recorded
//! with the change itself (ADR-0108 D8).
//!
//! The record rides in the statement's own transaction, so the two cannot come
//! apart: a cancelled change leaves no entry and a committed one always has its
//! entry. That is proved from both sides — a `CANCEL` that would leave an entry
//! behind, and a `COMMIT` that would lose one, each fail a test here.

#![allow(clippy::panic, clippy::unwrap_used)]

use std::sync::Arc;

use tessari_kv::{KvBackend, MemoryBackend};
use tessari_session::Session;
use tessari_storage::Store;
use tessari_types::Value;

const PASSWORD: &str = "correct horse battery";
const PLANTED: &str = "a-planted-password-7c41";

fn store() -> Store {
    Store::open(Arc::new(MemoryBackend::new()) as Arc<dyn KvBackend>).unwrap()
}

/// The trail's administration entries, as `(actor, statement, subject)`.
fn administered(store: &Store) -> Vec<(String, String, String)> {
    tessari_storage::audit_entries(store)
        .unwrap()
        .into_iter()
        .filter_map(|entry| {
            let Value::Object(fields) = entry else {
                panic!("an entry that is not an object");
            };
            if fields.get("kind") != Some(&Value::String("administration".into())) {
                return None;
            }
            let text = |name: &str| match fields.get(name) {
                Some(Value::String(text)) => text.clone(),
                other => panic!("`{name}` is {other:?}"),
            };
            assert!(fields.contains_key("at"), "no time on the entry");
            Some((text("actor"), text("statement"), text("subject")))
        })
        .collect()
}

fn owned(store: &Store) -> Session<'_> {
    Session::new(store)
        .run(&format!(
            "DEFINE NAMESPACE prod; USE NAMESPACE prod; DEFINE DATABASE work; USE DATABASE work;
             DEFINE TABLE orders SCHEMALESS;
             DEFINE USER root ROLE owner PASSWORD '{PASSWORD}';"
        ))
        .unwrap();
    let mut root = Session::new(store);
    root.sign_in("root", PASSWORD).unwrap();
    root.run("USE NAMESPACE prod; USE DATABASE work;").unwrap();
    root
}

#[test]
fn a_change_of_authority_is_recorded_naming_who_what_and_whom() {
    let store = store();
    let mut root = owned(&store);
    root.run(&format!(
        "DEFINE USER ada ON prod.work ROLE editor PASSWORD '{PASSWORD}';
         GRANT read ON orders TO ada;
         ALTER USER ada SET ROLE viewer;
         DROP USER ada;"
    ))
    .unwrap();

    // As a set: entries written in one millisecond share their time prefix and
    // the trail does not promise an order between them.
    let mut root_acts: Vec<_> = administered(&store)
        .into_iter()
        .filter(|(actor, ..)| actor == "root")
        .collect();
    root_acts.sort();
    assert_eq!(
        root_acts,
        [
            ("root", "ALTER USER", "ada"),
            ("root", "DEFINE USER", "ada"),
            ("root", "DROP USER", "ada"),
            ("root", "GRANT", "ada"),
        ]
        .map(|(actor, statement, subject)| (
            actor.to_owned(),
            statement.to_owned(),
            subject.to_owned()
        )),
    );
    // The first owner was declared by the anonymous session that could.
    assert!(
        administered(&store).contains(&(
            "anonymous".to_owned(),
            "DEFINE USER".to_owned(),
            "root".to_owned()
        )),
        "{:?}",
        administered(&store)
    );
}

#[test]
fn a_cancelled_change_leaves_no_entry_and_a_committed_one_does() {
    let store = store();
    let mut root = owned(&store);
    let before = administered(&store).len();

    root.run(&format!(
        "BEGIN; DEFINE USER gone ON prod.work ROLE viewer PASSWORD '{PASSWORD}'; CANCEL;"
    ))
    .unwrap();
    assert_eq!(
        administered(&store).len(),
        before,
        "a change that never happened was recorded"
    );

    root.run(&format!(
        "BEGIN; DEFINE USER kept ON prod.work ROLE viewer PASSWORD '{PASSWORD}'; COMMIT;"
    ))
    .unwrap();
    let after = administered(&store);
    assert_eq!(after.len(), before + 1, "{after:?}");
    assert!(
        after.contains(&(
            "root".to_owned(),
            "DEFINE USER".to_owned(),
            "kept".to_owned()
        )),
        "{after:?}"
    );
}

#[test]
fn the_entry_names_the_change_and_never_carries_the_password() {
    let store = store();
    let mut root = owned(&store);
    root.run(&format!(
        "DEFINE USER ada ON prod.work ROLE editor PASSWORD '{PLANTED}';
         ALTER USER ada SET PASSWORD '{PLANTED}';"
    ))
    .unwrap();
    let trail = format!("{:?}", tessari_storage::audit_entries(&store).unwrap());
    // The control: the trail holds the change, so a search that finds nothing
    // is a search that looked at something.
    assert!(trail.contains("ALTER USER"), "{trail}");
    assert!(
        !trail.contains(PLANTED),
        "the trail recorded a password: {trail}"
    );
}

#[test]
fn a_refused_change_leaves_no_entry() {
    // Held by the transaction rather than by the recording: a statement refused
    // inside a script discards the script's transaction, so an entry written
    // beside it goes too. Measured 2026-10-02 by recording before the success
    // check: this stayed green, because no path commits a refused statement's
    // writes. It is kept because that is the property an auditor relies on.
    let store = store();
    let mut root = owned(&store);
    root.run(&format!(
        "DEFINE USER ada ON prod.work ROLE viewer PASSWORD '{PASSWORD}';"
    ))
    .unwrap();
    let before = administered(&store).len();
    assert!(
        root.run(&format!(
            "BEGIN; DEFINE USER ada ON prod.work ROLE owner PASSWORD '{PASSWORD}'; COMMIT;"
        ))
        .is_err(),
        "a second user of one name was declared"
    );
    assert_eq!(
        administered(&store).len(),
        before,
        "{:?}",
        administered(&store)
    );
}
