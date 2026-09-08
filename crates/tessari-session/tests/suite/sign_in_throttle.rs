//! What a node does when somebody keeps guessing.
//!
//! The unit tests beside `throttle.rs` establish that the counter counts and
//! that the wait doubles. This file establishes the thing those cannot: that the
//! counter is actually **wired in front of** `sign_in`, on the path every surface
//! takes, and that it is reached before the password is checked rather than
//! after.
//!
//! # Why the assertion is on the error and not on a clock
//!
//! The finding's own wording is "refused **without reaching the hasher**", and
//! the obvious test for that is a timer: a throttled refusal is microseconds and
//! a real one is tens of milliseconds, four orders of magnitude apart. It is not
//! written that way. A lower bound on how long a *correct* implementation takes
//! is a bound on how slow the machine is allowed to be, and a loaded CI host
//! makes it fail for a reason that has nothing to do with this code.
//!
//! `SignInThrottled` and `SignInRefused` are produced at different places on that
//! path — the first before the store is even opened, the second only after
//! `verifies` has run and returned false — so which one comes back says which
//! half of the function was reached, and says it without consulting a clock.
//!
//! # Every test uses its own user name
//!
//! The counters are one table per process, which is what makes them survive a
//! reconnect and therefore what makes them work at all. Two tests failing against
//! the same name would count together. Distinct names keep them apart.

#![allow(clippy::panic, clippy::unwrap_used, clippy::indexing_slicing)]

use std::sync::Arc;

use tessari_constants::FREE_SIGN_IN_FAILURES;
use tessari_kv::{KvBackend, MemoryBackend};
use tessari_session::{Error, Session};
use tessari_storage::Store;

const PASSWORD: &str = "correct horse battery";

/// A store holding one user under `name`.
fn store_with(name: &str) -> Store {
    let backend = Arc::new(MemoryBackend::new()) as Arc<dyn KvBackend>;
    let store = Store::open(backend).unwrap();
    let mut opening = Session::new(&store);
    opening
        .run(&format!(
            "DEFINE USER {name} ROLE owner PASSWORD '{PASSWORD}';"
        ))
        .unwrap();
    drop(opening);
    store
}

#[test]
fn guessing_is_refused_outright_once_the_allowance_is_spent() {
    let store = store_with("adaline");
    let mut session = Session::new(&store);

    // Every miss inside the allowance is answered by the credential check —
    // `SignInRefused` is only reachable after `verifies` has run.
    for attempt in 1..FREE_SIGN_IN_FAILURES {
        let refused = session.sign_in("adaline", "wrong").unwrap_err();
        assert!(
            matches!(refused, Error::SignInRefused),
            "attempt {attempt} of the allowance was throttled instead of checked: {refused}"
        );
    }
    // The last one of the allowance is still checked, and spends it.
    assert!(matches!(
        session.sign_in("adaline", "wrong").unwrap_err(),
        Error::SignInRefused
    ));

    // And now the node stops looking. A different error, from a different place
    // in the function — which is what "without reaching the hasher" means here.
    let throttled = session.sign_in("adaline", "wrong").unwrap_err();
    assert!(
        matches!(throttled, Error::SignInThrottled),
        "the attempt past the allowance still reached the password check: {throttled}"
    );
}

#[test]
fn a_correct_password_is_refused_too_while_the_wait_stands() {
    // The property that makes this a throttle rather than a filter on wrong
    // passwords: once an identity is waiting, it is waiting. A guard that let
    // the right password through would be a guard an attacker steps past on the
    // attempt that matters.
    let store = store_with("bartholomew");
    let mut session = Session::new(&store);
    for _ in 0..FREE_SIGN_IN_FAILURES {
        assert!(session.sign_in("bartholomew", "wrong").is_err());
    }
    let held = session.sign_in("bartholomew", PASSWORD).unwrap_err();
    assert!(
        matches!(held, Error::SignInThrottled),
        "the wait was skipped for a password that happened to be right: {held}"
    );
}

#[test]
fn a_new_connection_inherits_the_wait() {
    // The reason the counters are not held per session. If they were, this test
    // would pass by signing in on a fresh `Session` — which is exactly what an
    // attacker does, and it costs them one reconnect.
    let store = store_with("charlemagne");
    let mut first = Session::new(&store);
    for _ in 0..FREE_SIGN_IN_FAILURES {
        assert!(first.sign_in("charlemagne", "wrong").is_err());
    }
    drop(first);

    let mut second = Session::new(&store);
    let throttled = second.sign_in("charlemagne", "wrong").unwrap_err();
    assert!(
        matches!(throttled, Error::SignInThrottled),
        "a reconnect got a fresh allowance: {throttled}"
    );
}

#[test]
fn a_name_that_does_not_exist_is_counted_like_any_other() {
    // Counting only names the catalog knows would turn the throttle into a
    // catalog oracle: an attacker would learn which names exist by watching
    // which ones start to wait. So the count is against the name that was tried.
    let store = store_with("desdemona");
    let mut session = Session::new(&store);
    for _ in 0..FREE_SIGN_IN_FAILURES {
        assert!(matches!(
            session.sign_in("nobody-of-that-name", "wrong").unwrap_err(),
            Error::SignInRefused
        ));
    }
    let throttled = session.sign_in("nobody-of-that-name", "wrong").unwrap_err();
    assert!(
        matches!(throttled, Error::SignInThrottled),
        "a name the catalog does not hold was never throttled: {throttled}"
    );
}

#[test]
fn signing_in_correctly_leaves_the_allowance_whole() {
    // The other half of the same rule, and the one a real user depends on: two
    // mistypes and a correct password must not leave the account one miss from a
    // wait tomorrow.
    let store = store_with("evangeline");
    let mut session = Session::new(&store);
    for _ in 1..FREE_SIGN_IN_FAILURES {
        assert!(session.sign_in("evangeline", "wrong").is_err());
    }
    session.sign_in("evangeline", PASSWORD).unwrap();

    for _ in 0..FREE_SIGN_IN_FAILURES {
        assert!(matches!(
            session.sign_in("evangeline", "wrong").unwrap_err(),
            Error::SignInRefused,
        ));
    }
}
