//! Asking a node that is serving for a backup.
//!
//! The gap this closes is about **access**, not consistency. `write_from`
//! already fixes the log's tail before reading a record and stops there, so a
//! write landing mid-backup is honestly outside the file. What was missing is
//! that it takes an in-process `&Store` — and a node that is serving holds that
//! handle, with the store single-writer beneath it, so no second process can
//! open it to take one.
//!
//! So the node is asked, in the language, and these tests ask it the way a
//! client would.

#![allow(clippy::panic, clippy::unwrap_used, clippy::indexing_slicing)]

use std::sync::Arc;

use tessari_kv::{KvBackend, MemoryBackend};
use tessari_session::{Outcome, Session};
use tessari_storage::Store;
use tessari_types::{RecordId, Value};

const PASSWORD: &str = "correct horse battery";

fn store() -> Store {
    let backend = Arc::new(MemoryBackend::new()) as Arc<dyn KvBackend>;
    Store::open(backend).unwrap()
}

/// Two records in a database, on a store nobody has closed.
fn ready(store: &Store) -> Session<'_> {
    let mut session = Session::new(store);
    session
        .run(
            "DEFINE NAMESPACE prod; USE NAMESPACE prod;\n\
             DEFINE DATABASE shop; USE DATABASE shop;\n\
             DEFINE COLLECTION orders;\n\
             CREATE orders:1 = { total: 3 };\n\
             CREATE orders:2 = { total: 7 };",
        )
        .unwrap();
    session
}

/// The bytes a `BACKUP` answered with.
fn taken(session: &mut Session<'_>, script: &str) -> Vec<u8> {
    let outcomes = session.run(script).unwrap();
    match outcomes.last() {
        Some(Outcome::Value(Value::Bytes(held))) => held.clone(),
        other => panic!("a backup answered with {other:?}"),
    }
}

fn ids(session: &mut Session<'_>, script: &str) -> Vec<RecordId> {
    let outcomes = session.run(script).unwrap();
    let mut found: Vec<RecordId> = outcomes
        .last()
        .unwrap()
        .records()
        .unwrap()
        .iter()
        .map(|(id, _)| id.clone())
        .collect();
    found.sort();
    found
}

#[test]
fn a_backup_taken_through_the_language_verifies_and_restores() {
    // The whole point, end to end: the file a serving node hands over is the
    // file `write` produces, so the verifier and the restorer need no second
    // shape to read.
    let source = store();
    let mut session = ready(&source);
    let held = taken(&mut session, "BACKUP;");

    let verified = tessari_backup::verify(&mut held.as_slice()).unwrap();
    assert!(verified.records > 0, "{verified:?}");

    let restored_store = store();
    tessari_backup::read(&restored_store, &mut held.as_slice()).unwrap();
    let mut restored = Session::new(&restored_store);
    restored
        .run("USE NAMESPACE prod; USE DATABASE shop;")
        .unwrap();
    assert_eq!(
        ids(&mut restored, "SELECT * FROM orders;"),
        vec![RecordId::Int(1), RecordId::Int(2)]
    );
}

#[test]
fn an_increment_through_the_language_is_refused_where_a_sequence_names_no_log() {
    // `BACKUP FROM n` gives the node one number, and a number counts in one log.
    // This store holds several — the store's own and the shop's — so sequence
    // `n` in each is a different moment, and a file bounded by one of them would
    // read as whole while missing the rest (Q-625).
    //
    // This test used to take an increment here and prove the two files together
    // were the whole store. It cannot any more, and the refusal is why: an
    // increment names a log, and `FROM n` has no way to. What replaces it is
    // `write_from`, which takes the log — exercised in `restore.rs` and
    // `catch_up.rs` — until the statement can name one too.
    let source = store();
    let mut session = ready(&source);
    let base = taken(&mut session, "BACKUP;");
    assert!(
        tessari_backup::verify(&mut base.as_slice())
            .unwrap()
            .logs
            .len()
            > 1
    );

    session.run("CREATE orders:3 = { total: 11 };").unwrap();
    let refused = session.run("BACKUP FROM 2;").unwrap_err();
    assert!(
        refused.to_string().contains("one sequence"),
        "refused for the wrong reason: {refused}"
    );

    // And the whole backup still answers with the whole store, which is the
    // half that did not change.
    let restored_store = store();
    tessari_backup::read(&restored_store, &mut base.as_slice()).unwrap();
    let mut restored = Session::new(&restored_store);
    restored
        .run("USE NAMESPACE prod; USE DATABASE shop;")
        .unwrap();
    assert_eq!(
        ids(&mut restored, "SELECT * FROM orders;"),
        vec![RecordId::Int(1), RecordId::Int(2)]
    );
}

#[test]
fn a_write_that_lands_after_the_backup_began_is_outside_it_and_does_not_damage_it() {
    // The property the file format already had, asserted from the outside now
    // that a running node is the thing being asked. A backup is a statement of
    // what the store held at a sequence, and the tail in its header is what makes
    // "outside" checkable rather than a claim.
    let source = store();
    let mut session = ready(&source);
    let held = taken(&mut session, "BACKUP;");
    session.run("CREATE orders:99 = { total: 1000 };").unwrap();

    // The file is whole, and the late write is simply not in it.
    let verified = tessari_backup::verify(&mut held.as_slice()).unwrap();
    assert!(!verified.truncated, "{verified:?}");
    let restored_store = store();
    tessari_backup::read(&restored_store, &mut held.as_slice()).unwrap();
    let mut restored = Session::new(&restored_store);
    restored
        .run("USE NAMESPACE prod; USE DATABASE shop;")
        .unwrap();
    assert_eq!(
        ids(&mut restored, "SELECT * FROM orders;"),
        vec![RecordId::Int(1), RecordId::Int(2)],
        "a write that landed after the backup began got into it"
    );
    // …and the node still holds it, so nothing was lost by being outside.
    assert_eq!(ids(&mut session, "SELECT * FROM orders;").len(), 3);
}

#[test]
fn only_an_owner_may_take_one() {
    // A backup is every record in the store, past every grant and every tenancy
    // boundary. There is no permission smaller than "may see all of it".
    let store = store();
    let mut open = ready(&store);
    open.run("DEFINE USER root ROLE owner PASSWORD 'correct horse battery';")
        .unwrap();

    let mut root = Session::new(&store);
    root.sign_in("root", PASSWORD).unwrap();
    root.run("DEFINE USER ada ON prod.shop ROLE editor PASSWORD 'correct horse battery';")
        .unwrap();
    root.run("DEFINE USER kay ON prod.shop ROLE viewer PASSWORD 'correct horse battery';")
        .unwrap();

    assert!(!taken(&mut root, "BACKUP;").is_empty());
    for name in ["ada", "kay"] {
        let mut other = Session::new(&store);
        other.sign_in(name, PASSWORD).unwrap();
        assert!(
            other.run("BACKUP;").is_err(),
            "{name} took a backup of the whole store"
        );
    }
}

#[test]
fn a_grant_can_never_permit_one() {
    // The refusal that would otherwise have been a *vacuous pass*: a backup
    // names no table, so a rule shaped "every table this statement names is
    // granted" is satisfied by an empty list. That is the shape of the defect a
    // `READ` falling through a catch-all produced, so it is refused by name.
    let store = store();
    let mut open = ready(&store);
    open.run("DEFINE USER root ROLE owner PASSWORD 'correct horse battery';")
        .unwrap();
    let mut root = Session::new(&store);
    root.sign_in("root", PASSWORD).unwrap();
    root.run("DEFINE USER gran ON prod.shop ROLE owner PASSWORD 'correct horse battery';")
        .unwrap();
    root.run("USE NAMESPACE prod; USE DATABASE shop; GRANT read ON orders TO gran;")
        .unwrap();

    // An **owner** by role, so the role check passes — and the grant is what
    // refuses, which is the case an emptiness-reads-as-permission bug would let
    // through.
    let mut granted = Session::new(&store);
    granted.sign_in("gran", PASSWORD).unwrap();
    let refused = granted.run("BACKUP;");
    assert!(refused.is_err(), "{refused:?}");
    assert!(
        format!("{}", refused.unwrap_err()).contains("gran"),
        "the refusal did not name who was refused"
    );
}

#[test]
fn an_open_store_answers_because_an_empty_one_must_stay_usable() {
    let store = store();
    let mut session = ready(&store);
    assert!(!taken(&mut session, "BACKUP;").is_empty());
}

#[test]
fn a_backup_is_the_whole_store_and_not_the_selected_namespace() {
    // The one statement whose scope is the store. A caller who has selected a
    // database gets everything anyway, and the restored store proves it by
    // holding the namespace the session was never pointed at.
    let source = store();
    let mut session = ready(&source);
    session
        .run(
            "DEFINE NAMESPACE other; USE NAMESPACE other;\n\
             DEFINE DATABASE archive; USE DATABASE archive;\n\
             DEFINE COLLECTION notes;\n\
             CREATE notes:1 = { body: 'elsewhere' };\n\
             USE NAMESPACE prod; USE DATABASE shop;",
        )
        .unwrap();
    let held = taken(&mut session, "BACKUP;");

    let restored_store = store();
    tessari_backup::read(&restored_store, &mut held.as_slice()).unwrap();
    let mut restored = Session::new(&restored_store);
    restored
        .run("USE NAMESPACE other; USE DATABASE archive;")
        .unwrap();
    assert_eq!(
        ids(&mut restored, "SELECT * FROM notes;"),
        vec![RecordId::Int(1)]
    );
}
