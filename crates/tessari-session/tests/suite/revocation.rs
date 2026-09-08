//! Taking authority back, and how long it takes to bind.
//!
//! # The question these answer
//!
//! Not *can it be revoked* — every statement that grants has one that takes
//! back, and that was true before this file existed. The question is **when the
//! taking-back reaches somebody who is already signed in**, which is the only
//! moment revocation is ever needed in.
//!
//! Grants were never the problem: they are read from the catalog inside each
//! statement's own transaction, so `REVOKE … ON TABLE` binds on the next one.
//! The user *record* was, because `sign_in` copied it once and nothing read it
//! again — so `ALTER USER`, `DROP USER` and `REVOKE … ON REACH` reached an open
//! connection never, and the window was the connection's lifetime. Both halves
//! are called permissions from outside and nothing distinguished them.
//!
//! Every test here therefore holds a session **open across** the statement that
//! takes authority away, and then runs the next statement on that same session.
//! A test that signed in again afterwards would pass with the bug present.

#![allow(clippy::panic, clippy::unwrap_used)]

use std::sync::Arc;

use tessari_kv::{KvBackend, MemoryBackend};
use tessari_session::Session;
use tessari_storage::Store;

const PASSWORD: &str = "correct horse battery";

fn store() -> Store {
    let backend = Arc::new(MemoryBackend::new()) as Arc<dyn KvBackend>;
    Store::open(backend).unwrap()
}

/// A store owner, a tenancy, and an editor inside it.
fn peopled(store: &Store) {
    let mut session = Session::new(store);
    session
        .run(
            "DEFINE NAMESPACE prod; USE NAMESPACE prod;\n\
             DEFINE DATABASE shop; USE DATABASE shop;\n\
             DEFINE COLLECTION orders;\n\
             DEFINE USER root ROLE owner PASSWORD 'correct horse battery';",
        )
        .unwrap();
    let mut root = signed_in(store, "root");
    root.run("DEFINE USER ada ON prod.shop ROLE editor PASSWORD 'correct horse battery';")
        .unwrap();
}

fn signed_in<'a>(store: &'a Store, name: &str) -> Session<'a> {
    let mut session = Session::new(store);
    session.sign_in(name, PASSWORD).unwrap();
    session
        .run("USE NAMESPACE prod; USE DATABASE shop;")
        .unwrap();
    session
}

#[test]
fn a_role_change_reaches_a_session_that_is_already_open() {
    let store = store();
    peopled(&store);

    // Open first, and kept open across the demotion below. That ordering is the
    // whole test: the session holds a copy of the record that is about to stop
    // being true.
    let mut ada = signed_in(&store, "ada");
    ada.run("CREATE orders:1 = { total: 5 };")
        .expect("an editor writes");

    signed_in(&store, "root")
        .run("ALTER USER ada SET ROLE viewer;")
        .unwrap();

    let refused = ada
        .run("CREATE orders:2 = { total: 5 };")
        .expect_err("the demotion did not reach the open session");
    assert!(refused.to_string().contains("write"), "{refused}");
}

#[test]
fn revoking_an_authority_reaches_a_session_that_is_already_open() {
    let store = store();
    peopled(&store);

    let mut root_declaring = signed_in(&store, "root");
    root_declaring
        .run(
            "DEFINE USER kim ON NAMESPACE prod AUTHORITIES read, write \
             PASSWORD 'correct horse battery';",
        )
        .unwrap();

    let mut kim = signed_in(&store, "kim");
    kim.run("CREATE orders:1 = { total: 5 };")
        .expect("write was granted");

    // The statement this goal exists to add, revoking what it granted. Before
    // the record was re-read this was the *least* revocable thing in the store:
    // an authority set lives in the user record, and the record was a copy.
    root_declaring
        .run("REVOKE write ON NAMESPACE prod FROM kim;")
        .unwrap();

    let refused = kim
        .run("CREATE orders:2 = { total: 5 };")
        .expect_err("the revocation did not reach the open session");
    assert!(refused.to_string().contains("write"), "{refused}");

    // And it took exactly what it named: reading was not part of the statement.
    kim.run("SELECT * FROM orders;").expect("read survives");
}

#[test]
fn dropping_a_user_reaches_a_session_that_is_already_open() {
    let store = store();
    peopled(&store);

    let mut ada = signed_in(&store, "ada");
    ada.run("SELECT * FROM orders;").expect("an editor reads");

    signed_in(&store, "root").run("DROP USER ada;").unwrap();

    // Not "you may not" but "I do not know you", which is a different answer and
    // the one a client needs in order to know whether signing in again would
    // help. It would not: there is nothing left to sign in as.
    let refused = ada
        .run("SELECT * FROM orders;")
        .expect_err("the drop did not reach the open session");
    assert!(refused.to_string().contains("signed-in"), "{refused}");
}

#[test]
fn a_session_does_not_re_authorize_against_its_own_uncommitted_change() {
    let store = store();
    peopled(&store);

    let mut root = signed_in(&store, "root");

    // Demoting yourself and then carrying on inside the same transaction. The
    // re-read opens its own transaction on purpose, so what it sees is committed
    // state — a session that re-authorized against its own uncommitted write
    // could refuse the rest of a script it has not decided to keep, and then
    // roll the change back and leave nothing to explain the refusal.
    root.run(
        "BEGIN;\n\
         ALTER USER root SET ROLE viewer;\n\
         CREATE orders:1 = { total: 5 };\n\
         COMMIT;",
    )
    .expect("the statement after the demotion is still the old root");

    // After the commit it binds, on the next statement, like every other change
    // to the record.
    let refused = root
        .run("CREATE orders:2 = { total: 5 };")
        .expect_err("the committed demotion did not bind");
    assert!(refused.to_string().contains("write"), "{refused}");
}

#[test]
fn the_statement_bound_is_one_statement_and_it_is_measured_rather_than_asserted() {
    let store = store();
    peopled(&store);

    let mut ada = signed_in(&store, "ada");
    ada.run("CREATE orders:1 = { total: 5 };").unwrap();

    let mut root = signed_in(&store, "root");
    let taken = std::time::Instant::now();
    root.run("ALTER USER ada SET ROLE viewer;").unwrap();
    ada.run("CREATE orders:2 = { total: 5 };")
        .expect_err("the demotion did not bind");
    let bound = taken.elapsed();

    // Printed rather than only asserted: the number is what goes in the
    // readiness checklist, and a bound nobody measured is not a bound. Run with
    // `--nocapture` to read it.
    println!("revocation → first refusal on an open session: {bound:?}");

    // The ceiling is deliberately far above anything this path can cost — one
    // point read and one refusal. It is here to catch the bound becoming
    // *unbounded* again, which is the defect this file exists for, and not to
    // measure the machine it runs on.
    assert!(
        bound < std::time::Duration::from_secs(1),
        "one statement took {bound:?}"
    );
}

#[test]
fn dropping_the_last_user_leaves_an_open_store_rather_than_a_locked_one() {
    let store = store();
    let mut session = Session::new(&store);
    session
        .run(
            "DEFINE NAMESPACE prod; USE NAMESPACE prod;\n\
             DEFINE DATABASE shop; USE DATABASE shop;\n\
             DEFINE COLLECTION orders;\n\
             DEFINE USER root ROLE owner PASSWORD 'correct horse battery';",
        )
        .unwrap();

    let mut root = signed_in(&store, "root");
    root.run("DROP USER root;").unwrap();

    // The session's record has gone, so it is nobody — and a store with no user
    // at all is open, which is what keeps an emptied store enterable. Asserted
    // rather than assumed, because it is the one place where losing an identity
    // widens what a session may do, and it should be a decision on the record
    // rather than a surprise.
    root.run("CREATE orders:1 = { total: 5 };")
        .expect("an open store runs anything");
}
