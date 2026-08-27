//! `THROW` — a script refusing itself.
//!
//! `IF` gave a script the ability to compute a decision; until this, it could
//! not act on one. Every refusal had to be a condition the store itself happened
//! to check, so a rule the store does not know — "this order is already paid" —
//! could be *detected* in the language and not *enforced* by it.
//!
//! The property that makes it worth having is not the message. It is that a
//! refusal inside a transaction discards the work above it: a guard clause that
//! let the writes before it stand would be a comment.

#![allow(clippy::panic, clippy::unwrap_used, clippy::indexing_slicing)]

use std::sync::Arc;

use tessari_kv::{KvBackend, MemoryBackend};
use tessari_session::Session;
use tessari_storage::Store;

fn store() -> Store {
    let backend = Arc::new(MemoryBackend::new()) as Arc<dyn KvBackend>;
    Store::open(backend).unwrap()
}

fn ready(store: &Store) -> Session<'_> {
    let mut session = Session::new(store);
    session
        .run(
            "DEFINE NAMESPACE prod; USE NAMESPACE prod;\n\
             DEFINE DATABASE shop; USE DATABASE shop;\n\
             DEFINE TABLE orders;\n\
             CREATE orders:1 = { paid: true, total: 30 };",
        )
        .unwrap();
    session
}

fn rows(session: &mut Session<'_>, statement: &str) -> usize {
    let outcome = session.run(statement).unwrap();
    let last = outcome.last().expect("an outcome");
    last.records().expect("records").len()
}

#[test]
fn a_script_may_refuse_itself() {
    let store = store();
    let mut session = ready(&store);

    let refused = session.run("THROW 'this order is already paid';");

    let message = format!("{:?}", refused.expect_err("THROW must fail"));
    assert!(
        message.contains("this order is already paid"),
        "the message the script chose must reach the caller: {message}"
    );
}

#[test]
fn a_refusal_carries_the_message_as_written() {
    let store = store();
    let mut session = ready(&store);

    // A string is used as it was typed rather than re-quoted, because the
    // message is for a person and the quotes are syntax.
    let message = session
        .run("THROW 'nope';")
        .expect_err("THROW must fail")
        .to_string();
    assert!(message.starts_with("nope"), "got {message}");
}

#[test]
fn a_thrown_value_need_not_be_a_string() {
    let store = store();
    let mut session = ready(&store);

    assert!(session.run("THROW 42;").is_err());
}

#[test]
fn a_refusal_is_computed_and_may_read_the_store() {
    let store = store();
    let mut session = ready(&store);

    // The point of the statement: the store does not know what "already paid"
    // means, and now the script can enforce it anyway.
    let refused = session.run(
        "LET $paid = (SELECT * FROM orders WHERE paid = true);\n\
         THROW IF $paid != [] THEN 'already paid' ELSE 'fine' END;",
    );

    let message = format!("{:?}", refused.expect_err("THROW must fail"));
    assert!(message.contains("already paid"), "got {message}");
}

#[test]
fn a_guard_discards_the_writes_above_it() {
    let store = store();
    let mut session = ready(&store);

    // Without this property the statement would be a comment: the whole reason
    // to write a guard is that reaching it undoes what came before.
    let refused = session.run(
        "BEGIN;\n\
         CREATE orders:2 = { paid: false, total: 10 };\n\
         THROW 'changed my mind';\n\
         COMMIT;",
    );
    assert!(refused.is_err());

    assert_eq!(
        rows(&mut session, "SELECT * FROM orders;"),
        1,
        "the create above the refusal must not have landed"
    );
}

#[test]
fn a_refusal_that_is_not_reached_changes_nothing() {
    let store = store();
    let mut session = ready(&store);

    // `IF` is an expression, so the guard is a value the `THROW` never receives
    // — there is no arm here that runs and no statement that is skipped. What is
    // pinned is that a script holding a `THROW` is not thereby a script that
    // fails, which is the shape a reader will assume.
    session
        .run("CREATE orders:3 = { paid: false, total: 5 };")
        .unwrap();

    assert_eq!(rows(&mut session, "SELECT * FROM orders;"), 2);
}
