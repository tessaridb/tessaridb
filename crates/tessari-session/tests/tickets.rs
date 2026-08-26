//! Signing in once and carrying the result.
//!
//! A password check is expensive on purpose — nineteen mebibytes and tens of
//! milliseconds — and a surface with no notion of a conversation pays it on
//! every request. A ticket is what a session hands out instead: proof that a
//! sign-in already happened, and of exactly whom.
//!
//! Two properties decide whether that is safe, and both are tested here.
//!
//! **A ticket dies with the authority it stands for.** Taking one up re-reads
//! the user from the catalog and requires the record to be unchanged, so a
//! password rotation, a role correction and a removal each kill every ticket of
//! that user. This is the property a revocation list would have had to
//! maintain from inside every statement that touches a user — including the
//! ones not written yet — and comparison has nothing to forget. `ALTER USER`
//! landed a day before this file and already moves two fields.
//!
//! **A ticket carries no authority the password did not.** What is taken up is
//! the same user record, so a viewer's ticket is a viewer. Grants are
//! deliberately not part of it: `within_grants` reads them per statement, so a
//! `GRANT` narrows a ticket-borne session on its very next statement rather
//! than at some later renewal.

#![allow(clippy::panic, clippy::unwrap_used)]

use std::sync::Arc;
use std::time::Instant;

use tessari_kv::{KvBackend, MemoryBackend};
use tessari_session::{Session, Ticket};
use tessari_storage::Store;

const PASSWORD: &str = "correct horse battery";

fn store() -> Store {
    let backend = Arc::new(MemoryBackend::new()) as Arc<dyn KvBackend>;
    Store::open(backend).unwrap()
}

/// A store with a tenancy, a store owner, an editor and a viewer.
fn peopled(store: &Store) {
    let mut session = Session::new(store);
    session
        .run(
            "DEFINE NAMESPACE prod; USE NAMESPACE prod;\n\
             DEFINE DATABASE shop; USE DATABASE shop;\n\
             DEFINE TABLE orders;",
        )
        .unwrap();
    session
        .run("DEFINE USER root ROLE owner PASSWORD 'correct horse battery';")
        .unwrap();
    let mut root = signed_in(store, "root");
    root.run(
        "DEFINE USER ada ON prod.shop ROLE editor PASSWORD 'correct horse battery';\n\
         DEFINE USER vic ON prod.shop ROLE viewer PASSWORD 'correct horse battery';",
    )
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

/// The ticket a fresh sign-in as `name` hands out.
fn ticket_for(store: &Store, name: &str) -> Ticket {
    signed_in(store, name).ticket().unwrap()
}

/// A session that took `ticket` up, selected onto the tenancy.
fn resumed<'a>(store: &'a Store, ticket: &Ticket) -> Session<'a> {
    let mut session = Session::new(store);
    session.resume(ticket).unwrap();
    session
        .run("USE NAMESPACE prod; USE DATABASE shop;")
        .unwrap();
    session
}

#[test]
fn a_ticket_taken_up_again_is_the_same_identity() {
    let store = store();
    peopled(&store);

    let ticket = ticket_for(&store, "ada");
    let mut carried = resumed(&store, &ticket);

    // An editor writes. If the resumed session were anonymous against a closed
    // store this would be refused, and if it were nobody at all it would not
    // reach the table.
    carried
        .run("CREATE orders:1 = { total: 5 };")
        .expect("an editor's ticket writes as an editor");
}

#[test]
fn the_same_ticket_serves_more_than_once() {
    let store = store();
    peopled(&store);

    // The whole point: one sign-in, many requests. A ticket that worked once
    // and then had to be re-issued would have moved the cost rather than
    // removed it.
    let ticket = ticket_for(&store, "ada");
    for total in 1..=3_u32 {
        let mut carried = resumed(&store, &ticket);
        carried
            .run(&format!("CREATE orders:{total} = {{ total: {total} }};"))
            .unwrap();
    }
}

#[test]
fn an_anonymous_session_has_nothing_to_hand_out() {
    let store = store();
    peopled(&store);

    // Not "an empty ticket" — no ticket. A surface that could mint one for
    // nobody would be minting a credential out of the absence of one.
    assert!(Session::new(&store).ticket().is_none());
}

#[test]
fn a_password_change_kills_the_ticket() {
    let store = store();
    peopled(&store);

    let ticket = ticket_for(&store, "ada");
    // It works before, so the refusal below is the change and not the setup.
    assert!(Session::new(&store).resume(&ticket).is_ok());

    let mut root = signed_in(&store, "root");
    root.run("ALTER USER ada SET PASSWORD 'a different horse entirely';")
        .unwrap();

    // Rotating a password that somebody else may have learned is worthless if
    // a token minted with the old one keeps working.
    assert!(
        Session::new(&store).resume(&ticket).is_err(),
        "a rotated password must not leave a live token behind"
    );
}

#[test]
fn a_role_change_kills_the_ticket() {
    let store = store();
    peopled(&store);

    let ticket = ticket_for(&store, "ada");
    assert!(Session::new(&store).resume(&ticket).is_ok());

    let mut root = signed_in(&store, "root");
    root.run("ALTER USER ada SET ROLE viewer;").unwrap();

    // Demoting somebody who holds a token has to take effect, or the demotion
    // is a note in the catalog rather than a change in what they may do.
    assert!(
        Session::new(&store).resume(&ticket).is_err(),
        "a demotion must not leave the old authority reachable"
    );
}

#[test]
fn removing_the_user_kills_the_ticket() {
    let store = store();
    peopled(&store);

    let ticket = ticket_for(&store, "ada");
    assert!(Session::new(&store).resume(&ticket).is_ok());

    let mut root = signed_in(&store, "root");
    root.run("DROP USER ada;").unwrap();

    assert!(
        Session::new(&store).resume(&ticket).is_err(),
        "a removed user must not be reachable through a token"
    );
}

#[test]
fn a_ticket_carries_no_authority_the_password_did_not() {
    let store = store();
    peopled(&store);

    let ticket = ticket_for(&store, "vic");
    let mut carried = resumed(&store, &ticket);

    // A viewer reads and does not write, through a token exactly as through a
    // password. A token that widened anything would be a second permission
    // system quietly disagreeing with the first.
    carried.run("SELECT * FROM orders;").unwrap();
    assert!(
        carried.run("CREATE orders:9 = { total: 9 };").is_err(),
        "a viewer's ticket must not write"
    );
    assert!(
        carried
            .run("DEFINE USER mallory ON prod.shop ROLE owner PASSWORD 'a long enough one';")
            .is_err(),
        "a viewer's ticket must not declare users"
    );
}

#[test]
fn two_tickets_are_never_the_same_token() {
    let store = store();
    peopled(&store);

    // Issued to the same user, moments apart. A token a caller can predict from
    // one they hold is not a token.
    let first = ticket_for(&store, "ada");
    let second = ticket_for(&store, "ada");
    assert_ne!(first.bearer(), second.bearer());
    assert!(
        first.bearer().len() >= 32,
        "a token short enough to guess is not worth issuing"
    );
}

#[test]
fn taking_a_ticket_up_does_not_pay_for_a_password() {
    let store = store();
    peopled(&store);

    let ticket = ticket_for(&store, "ada");

    let signing = Instant::now();
    Session::new(&store).sign_in("ada", PASSWORD).unwrap();
    let signing = signing.elapsed();

    let taking = Instant::now();
    Session::new(&store).resume(&ticket).unwrap();
    let taking = taking.elapsed();

    // This is the whole reason the type exists, so it is asserted rather than
    // assumed. The real gap is three orders of magnitude — a memory-hard hash
    // against a catalog read — and the margin here is deliberately loose so
    // that a loaded machine cannot fail it while a regression that started
    // hashing again still does.
    assert!(
        taking.saturating_mul(5) < signing,
        "resume {taking:?} was not decisively cheaper than sign-in {signing:?}"
    );
}
