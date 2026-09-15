//! A catalog record must never be indexed as though it were a row of a user's table.
//!
//! Index maintenance picks the definitions a mutation implies, and it picked them
//! by `TableId` alone. A `TableId` is not unique on its own: it is allocated
//! store-wide from 1, and the system catalog reserves the first eighteen at
//! namespace 0, database 0 — `NAMESPACES` is 1, `DATABASES` is 2, `TABLES` is 3.
//! So the first eighteen tables anybody declares carry ids that a system table
//! also carries, and a write to the catalog selected the index definitions of
//! whatever user table shared its number.
//!
//! The entries then landed in the **user's** keyspace, because the address is
//! built from the definition — correctly. The two halves are what add up to the
//! wrong answer: chosen with a partial key, applied with the whole one.
//!
//! Both directions are covered here because the second was found while narrowing
//! the first, and it is the one with no tenancy in it at all: an ordinary row
//! refused because somebody, somewhere, created a *database* with that name.
//!
//! Two details below are load-bearing and both were established by measurement,
//! because the first version of this file passed against the defect.
//!
//! A pad table goes ahead of the indexed one, so the indexed table is the store's
//! **second** and lands on `DATABASES`. And the indexed field is called `name`,
//! which is what a database's catalog record carries: a record lacking the field
//! projects nothing and collides with nobody, so an index on `email` sees this
//! defect and reports it green. Change either and these tests stop testing.

#![allow(clippy::panic, clippy::unwrap_used)]

use std::sync::Arc;

use tessari_kv::{KvBackend, MemoryBackend};
use tessari_session::{Outcome, Session};
use tessari_storage::Store;

fn store() -> Store {
    let backend = Arc::new(MemoryBackend::new()) as Arc<dyn KvBackend>;
    Store::open(backend).unwrap()
}

/// A store whose **second** table is `people`, carrying a unique index on `name`.
///
/// `pad` exists only to push `people` onto the id the system catalog uses for
/// `DATABASES`, and the field is `name` because that is the field a database's
/// own catalog record holds. A reader who removes either will find these tests
/// green and the defect back.
fn with_an_indexed_second_table(session: &mut Session<'_>) {
    session
        .run(
            "DEFINE NAMESPACE first; USE NAMESPACE first;\n\
             DEFINE DATABASE main; USE DATABASE main;\n\
             DEFINE TABLE pad SCHEMALESS;\n\
             DEFINE TABLE people SCHEMAFULL;\n\
             DEFINE FIELD name ON people TYPE string;\n\
             DEFINE INDEX people_name ON people FIELDS name UNIQUE;",
        )
        .unwrap();
}

/// How many records one statement answered with.
fn rows(session: &mut Session<'_>, statement: &str) -> usize {
    match session.run(statement).unwrap().pop() {
        Some(Outcome::Records { records, .. }) => records.len(),
        other => panic!("expected records, got {other:?}"),
    }
}

#[test]
fn two_databases_may_share_a_name_in_different_namespaces() {
    let store = store();
    let mut session = Session::new(&store);
    with_an_indexed_second_table(&mut session);

    session.run("DEFINE DATABASE shared_name;").unwrap();

    let second =
        session.run("DEFINE NAMESPACE second; USE NAMESPACE second; DEFINE DATABASE shared_name;");
    assert!(
        second.is_ok(),
        "a database in one namespace refused a database in another, which is the \
         user's index enforcing itself over the catalog: {second:?}",
    );
}

#[test]
fn a_row_may_hold_a_value_that_is_also_a_database_name() {
    let store = store();
    let mut session = Session::new(&store);
    with_an_indexed_second_table(&mut session);

    session
        .run("DEFINE DATABASE payroll; USE DATABASE main;")
        .unwrap();

    let written = session.run("CREATE people:'p1' = { name: 'payroll' };");
    assert!(
        written.is_ok(),
        "an application row was refused because a DATABASE holds that name, and \
         the table it was refused from is empty: {written:?}",
    );
    assert_eq!(
        rows(&mut session, "SELECT * FROM people WHERE name = 'payroll';"),
        1,
        "the row was accepted but its own index cannot find it",
    );
}

#[test]
fn the_unique_index_still_refuses_a_genuine_duplicate() {
    let store = store();
    let mut session = Session::new(&store);
    with_an_indexed_second_table(&mut session);

    session
        .run("CREATE people:'p1' = { name: 'ada' };")
        .unwrap();

    let again = session.run("CREATE people:'p2' = { name: 'ada' };");
    assert!(
        again.is_err(),
        "two rows took one value in a UNIQUE index — the fix stopped maintaining \
         it rather than scoping it: {again:?}",
    );
}
