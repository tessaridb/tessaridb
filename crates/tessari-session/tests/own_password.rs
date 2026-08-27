//! Changing your own password.
//!
//! # Why this is not `ALTER USER`
//!
//! `ALTER USER … SET PASSWORD` is administering somebody, and administering
//! needs an owner who administers the tenancy they are in. That is right for
//! *somebody else's* credential and leaves a hole for your own: a `viewer` or an
//! `editor` whose password may have leaked cannot rotate it at all, and has to
//! find an owner — who then knows their new password.
//!
//! # Why it needs the current password
//!
//! Because a session is not proof of a password. A token can be copied off a
//! plaintext connection or out of a log, and if holding one were enough to
//! change the password, a stolen token would be a permanent takeover: the
//! thief locks the owner out and there is no way back that is not a restore
//! from backup.
//!
//! So this asks for the current password and verifies it, which also makes it
//! the one place a caller may act on a user without administering them — the
//! subject is always the caller, so there is no subject to bound.

#![allow(clippy::panic, clippy::unwrap_used)]

use std::sync::Arc;

use tessari_kv::{KvBackend, MemoryBackend};
use tessari_session::Session;
use tessari_storage::Store;

const PASSWORD: &str = "correct horse battery";
const ROTATED: &str = "a different horse entirely";

fn store() -> Store {
    let backend = Arc::new(MemoryBackend::new()) as Arc<dyn KvBackend>;
    Store::open(backend).unwrap()
}

fn peopled(store: &Store) {
    let mut session = Session::new(store);
    session
        .run(
            "DEFINE NAMESPACE prod; USE NAMESPACE prod;\n\
             DEFINE DATABASE shop; USE DATABASE shop; DEFINE TABLE orders;",
        )
        .unwrap();
    session
        .run("DEFINE USER root ROLE owner PASSWORD 'correct horse battery';")
        .unwrap();
    let mut root = signed_in(store, "root", PASSWORD);
    root.run(
        "DEFINE USER ada ON prod.shop ROLE editor PASSWORD 'correct horse battery';\n\
         DEFINE USER vic ON prod.shop ROLE viewer PASSWORD 'correct horse battery';",
    )
    .unwrap();
}

fn signed_in<'a>(store: &'a Store, name: &str, password: &str) -> Session<'a> {
    let mut session = Session::new(store);
    session.sign_in(name, password).unwrap();
    session
}

fn signs_in(store: &Store, name: &str, password: &str) -> bool {
    Session::new(store).sign_in(name, password).is_ok()
}

#[test]
fn a_viewer_may_rotate_their_own_password() {
    let store = store();
    peopled(&store);

    // The gap this closes. `vic` may read and nothing else, and before this
    // could not change their own credential without an owner doing it for them
    // — which means an owner choosing it, and knowing it.
    let mut vic = signed_in(&store, "vic", PASSWORD);
    vic.change_password(PASSWORD, ROTATED).unwrap();

    assert!(signs_in(&store, "vic", ROTATED));
    assert!(
        !signs_in(&store, "vic", PASSWORD),
        "the old one still works"
    );
}

#[test]
fn an_editor_may_too_and_the_role_is_untouched() {
    let store = store();
    peopled(&store);

    let mut ada = signed_in(&store, "ada", PASSWORD);
    ada.change_password(PASSWORD, ROTATED).unwrap();

    // The other field is exercised rather than read back: reading the catalog
    // would assert the value that was just written and prove nothing about
    // whether it still works.
    let mut ada = signed_in(&store, "ada", ROTATED);
    ada.run("USE NAMESPACE prod; USE DATABASE shop; CREATE orders:1 = { total: 5 };")
        .unwrap();
}

#[test]
fn the_current_password_is_required_and_checked() {
    let store = store();
    peopled(&store);

    // A signed-in session is not proof of a password. If it were, a token
    // copied off a plaintext connection would be a permanent takeover — the
    // thief sets a new password and the owner's way back is a restore.
    let mut vic = signed_in(&store, "vic", PASSWORD);
    assert!(vic.change_password("not the password", ROTATED).is_err());

    assert!(
        signs_in(&store, "vic", PASSWORD),
        "the password moved anyway"
    );
    assert!(!signs_in(&store, "vic", ROTATED));
}

#[test]
fn it_changes_the_caller_and_nobody_else() {
    let store = store();
    peopled(&store);

    let mut vic = signed_in(&store, "vic", PASSWORD);
    vic.change_password(PASSWORD, ROTATED).unwrap();

    // There is no subject to name, which is exactly why this needs no
    // containment check: the only account it can reach is the caller's own.
    assert!(signs_in(&store, "ada", PASSWORD));
    assert!(signs_in(&store, "root", PASSWORD));
}

#[test]
fn an_anonymous_session_has_no_password_to_change() {
    let store = store();
    peopled(&store);

    // Not "an empty password" and not a way in. On a closed store there is
    // nobody to be, and on an open one there is nobody to be either.
    let mut nobody = Session::new(&store);
    assert!(nobody.change_password(PASSWORD, ROTATED).is_err());
}

#[test]
fn a_user_removed_since_signing_in_cannot_rotate_anything() {
    let store = store();
    peopled(&store);

    // The session holds the user it signed in as, so the record is re-read
    // rather than trusted — the same reason a ticket is re-read. Otherwise a
    // session open across a `DROP USER` could write a hash back over an id the
    // catalog no longer holds.
    let mut vic = signed_in(&store, "vic", PASSWORD);
    let mut root = signed_in(&store, "root", PASSWORD);
    root.run("DROP USER vic;").unwrap();

    assert!(vic.change_password(PASSWORD, ROTATED).is_err());
    assert!(!signs_in(&store, "vic", ROTATED));
}

#[test]
fn rotating_your_own_password_still_needs_the_new_one_to_be_a_password() {
    let store = store();
    peopled(&store);

    // The same rule `DEFINE USER` applies, applied here too rather than left to
    // whichever path happened to check it.
    let mut vic = signed_in(&store, "vic", PASSWORD);
    assert!(
        vic.change_password(PASSWORD, "").is_err(),
        "an empty password was accepted"
    );
    assert!(signs_in(&store, "vic", PASSWORD));
}
