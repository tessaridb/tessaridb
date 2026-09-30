//! `BACKUP … OF` — a script of chosen namespaces and databases.
//!
//! The claims: restored into an empty store, a partial script holds the chosen
//! places whole — their definitions, records and the analyzers their fields
//! use — and nothing else: no other database, no unused analyzer, no user. Its
//! header says it is a part. A place the store does not hold is refused by name.

#![allow(clippy::panic, clippy::unwrap_used)]

use std::sync::Arc;

use tessari_kv::{KvBackend, MemoryBackend};
use tessari_session::{Error, Outcome, Session};
use tessari_storage::Store;
use tessari_types::Value;

const PASSWORD: &str = "a long one";

fn empty() -> Store {
    let backend: Arc<dyn KvBackend> = Arc::new(MemoryBackend::new());
    Store::open(backend).unwrap()
}

fn store() -> Store {
    let store = empty();
    Session::new(&store)
        .run(
            "DEFINE ANALYZER used FILTERS lowercase;
             DEFINE ANALYZER unused FILTERS lowercase;
             DEFINE NAMESPACE prod; USE NAMESPACE prod;
             DEFINE DATABASE orders; USE DATABASE orders;
             DEFINE TABLE items SCHEMALESS;
             DEFINE FIELD note ON items TYPE string ANALYZER used;
             CREATE items:1 = { note: 'Kept' };
             DEFINE DATABASE billing; USE DATABASE billing;
             DEFINE COLLECTION invoices; CREATE invoices:1 = { n: 1 };
             DEFINE NAMESPACE crm; USE NAMESPACE crm;
             DEFINE DATABASE people; USE DATABASE people;
             DEFINE COLLECTION contacts; CREATE contacts:1 = { n: 'x' };
             DEFINE DATABASE leads; USE DATABASE leads;
             DEFINE COLLECTION leads; CREATE leads:1 = { n: 'y' };
             DEFINE NAMESPACE other; USE NAMESPACE other;
             DEFINE DATABASE x; USE DATABASE x;
             DEFINE COLLECTION y; CREATE y:1 = { n: 2 };
             BEGIN;
             DEFINE USER root ROLE owner PASSWORD 'a long one';
             COMMIT;",
        )
        .unwrap();
    store
}

fn owner(store: &Store) -> Session<'_> {
    let mut session = Session::new(store);
    session.sign_in("root", PASSWORD).unwrap();
    session
}

fn script(session: &mut Session<'_>, statement: &str) -> String {
    match session.run(statement).unwrap().pop() {
        Some(Outcome::Value(Value::String(text))) => text,
        other => panic!("{statement} answered {other:?}"),
    }
}

/// How many records a read answers, or `None` when the place is refused.
fn held(session: &mut Session<'_>, place: &str, table: &str) -> Option<usize> {
    session.run(place).ok()?;
    match session.run(&format!("SELECT * FROM {table};")).ok()?.pop() {
        Some(Outcome::Records { records, .. }) => Some(records.len()),
        other => panic!("SELECT answered {other:?}"),
    }
}

#[test]
fn a_partial_script_restores_the_chosen_places_and_nothing_else() {
    let source = store();
    let text = script(
        &mut owner(&source),
        "BACKUP SCRIPT OF prod.orders, NAMESPACE crm;",
    );
    assert!(
        text.lines()
            .take(8)
            .any(|line| line.contains("a PART of the store")),
        "the header does not say the script is a part:\n{text}"
    );

    let restored = empty();
    let mut session = Session::new(&restored);
    session.run(&text).unwrap();

    assert_eq!(
        held(
            &mut session,
            "USE NAMESPACE prod; USE DATABASE orders;",
            "items"
        ),
        Some(1)
    );
    assert_eq!(
        held(
            &mut session,
            "USE NAMESPACE crm; USE DATABASE people;",
            "contacts"
        ),
        Some(1)
    );
    assert_eq!(
        held(
            &mut session,
            "USE NAMESPACE crm; USE DATABASE leads;",
            "leads"
        ),
        Some(1)
    );
    assert_eq!(
        held(
            &mut session,
            "USE NAMESPACE prod; USE DATABASE billing;",
            "invoices"
        ),
        None,
        "a database of a chosen namespace that was not chosen came across"
    );
    assert_eq!(
        held(&mut session, "USE NAMESPACE other; USE DATABASE x;", "y"),
        None,
        "a namespace that was not chosen came across"
    );
    // The analyzer the carried field uses came with it; the other did not.
    assert!(
        session
            .run("DEFINE ANALYZER used FILTERS lowercase;")
            .is_err()
    );
    assert!(
        session
            .run("DEFINE ANALYZER unused FILTERS lowercase;")
            .is_ok()
    );
    // No user came across, so the restored store is still open.
    assert!(session.run("INFO FOR USERS;").is_ok());
    assert!(
        !text.contains("DEFINE USER"),
        "a partial script carries a user"
    );
}

#[test]
fn a_place_the_store_does_not_hold_is_refused_by_name() {
    let source = store();
    let mut root = owner(&source);

    for statement in [
        "BACKUP SCRIPT OF NAMESPACE nowhere;",
        "BACKUP SCRIPT OF prod.nowhere;",
    ] {
        let error = root.run(statement).unwrap_err();
        assert!(
            matches!(error, Error::Unknown { .. }),
            "{statement} was refused as {error:?}"
        );
    }
}
