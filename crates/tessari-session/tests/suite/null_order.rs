//! `ORDER BY … NULLS FIRST | NULLS LAST` (ADR-0134 D8).

#![allow(clippy::panic, clippy::unwrap_used, clippy::indexing_slicing)]

use std::sync::Arc;
use tessari_session::Now;
use tessari_session::RunNow;

use tessari_kv::{KvBackend, MemoryBackend};
use tessari_session::{Outcome, Session};
use tessari_storage::Store;
use tessari_types::Value;

fn store() -> Store {
    let backend = Arc::new(MemoryBackend::new()) as Arc<dyn KvBackend>;
    Store::open(backend).now().unwrap()
}

/// Five tasks: two with a due day, one with `NULL`, two with none at all; the
/// due day is indexed, so an index walk could be asked for the order.
fn ready(store: &Store) -> Session<'_> {
    let mut session = Session::new(store);
    session
        .run_now(
            "DEFINE NAMESPACE prod; USE NAMESPACE prod;\n\
             DEFINE DATABASE work; USE DATABASE work;\n\
             DEFINE COLLECTION tasks;\n\
             DEFINE INDEX by_due ON tasks FIELDS due;\n\
             CREATE tasks:1 = { name: 'a', due: 2 };\n\
             CREATE tasks:2 = { name: 'b' };\n\
             CREATE tasks:3 = { name: 'c', due: 1 };\n\
             CREATE tasks:4 = { name: 'd', due: NULL };\n\
             CREATE tasks:5 = { name: 'e' };",
        )
        .unwrap();
    session
}

/// The names a read answers, in answer order.
fn names(session: &mut Session<'_>, statement: &str) -> Vec<String> {
    match session.run_now(statement).unwrap().pop().unwrap() {
        Outcome::Records { records, .. } => records
            .into_iter()
            .map(|(_, row)| match row {
                Value::Object(fields) => match fields.get("name") {
                    Some(Value::String(name)) => name.clone(),
                    other => panic!("a name, got {other:?}"),
                },
                other => panic!("a row, got {other:?}"),
            })
            .collect(),
        other => panic!("records, got {other:?}"),
    }
}

#[test]
fn without_the_clause_the_absences_are_lowest() {
    let store = store();
    let mut session = ready(&store);

    // `NONE` (by id) then `NULL` then the present values.
    assert_eq!(
        names(&mut session, "SELECT name FROM tasks ORDER BY due;"),
        ["b", "e", "d", "c", "a"]
    );
    assert_eq!(
        names(&mut session, "SELECT name FROM tasks ORDER BY due DESC;"),
        ["a", "c", "d", "b", "e"]
    );
}

#[test]
fn nulls_last_moves_the_absences_after_an_ascending_order() {
    let store = store();
    let mut session = ready(&store);

    // Together they keep their own order: `NONE`, then `NULL`.
    assert_eq!(
        names(
            &mut session,
            "SELECT name FROM tasks ORDER BY due NULLS LAST;"
        ),
        ["c", "a", "b", "e", "d"]
    );
}

#[test]
fn nulls_first_moves_them_before_a_descending_order() {
    let store = store();
    let mut session = ready(&store);

    assert_eq!(
        names(
            &mut session,
            "SELECT name FROM tasks ORDER BY due DESC NULLS FIRST;"
        ),
        ["b", "e", "d", "a", "c"]
    );
}

#[test]
fn the_clause_that_says_the_declared_order_changes_nothing() {
    let store = store();
    let mut session = ready(&store);

    assert_eq!(
        names(
            &mut session,
            "SELECT name FROM tasks ORDER BY due NULLS FIRST;"
        ),
        names(&mut session, "SELECT name FROM tasks ORDER BY due;")
    );
    assert_eq!(
        names(
            &mut session,
            "SELECT name FROM tasks ORDER BY due DESC NULLS LAST;"
        ),
        ["a", "c", "d", "b", "e"]
    );
}

#[test]
fn a_bounded_read_over_the_index_takes_the_present_values_first() {
    let store = store();
    let mut session = ready(&store);

    // An ascending walk of `by_due` would have reached the two absences that
    // hold no entry only by reading the table; the read sorts instead, so the
    // page is the right two.
    assert_eq!(
        names(
            &mut session,
            "SELECT * FROM tasks ORDER BY due NULLS LAST LIMIT 2;"
        ),
        ["c", "a"]
    );
    assert_eq!(
        names(
            &mut session,
            "SELECT * FROM tasks ORDER BY due DESC NULLS FIRST LIMIT 2;"
        ),
        ["b", "e"]
    );
}

#[test]
fn nulls_needs_first_or_last() {
    let store = store();
    let mut session = ready(&store);

    let refused = session
        .run_now("SELECT name FROM tasks ORDER BY due NULLS;")
        .unwrap_err()
        .to_string();

    assert!(
        refused.contains("`FIRST` or `LAST` after `NULLS`"),
        "{refused}"
    );
}

#[test]
fn a_field_called_nulls_is_still_a_key() {
    let store = store();
    let mut session = ready(&store);
    session
        .run_now("CREATE tasks:6 = { name: 'f', nulls: 0 };")
        .unwrap();

    let ordered = names(
        &mut session,
        "SELECT name FROM tasks ORDER BY nulls DESC LIMIT 1;",
    );

    assert_eq!(ordered, ["f"]);
}
