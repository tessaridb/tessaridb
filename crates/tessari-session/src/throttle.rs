//! How often this node will let anyone try a password.
//!
//! # Two bounds, because there are two attacks
//!
//! *Guessing.* Nothing counted failures, so a reachable port meant unlimited
//! attempts against every account. That is closed by a per-identity failure
//! count with a doubling delay.
//!
//! *Amplification.* This is the surprising one. A password hash is expensive **on
//! purpose** — nineteen mebibytes and tens of milliseconds — because that is what
//! makes a stolen catalog slow to crack. Unbounded and online, the same property
//! runs the wrong way: one TCP write costs the attacker nothing and costs this
//! node nineteen mebibytes, and **no attempt has to be valid**, so no credential
//! is needed to spend it. A few hundred simultaneous attempts is several
//! gigabytes. That is closed by a ceiling on concurrent verifications.
//!
//! Neither closes the other. A per-identity delay does not bound memory, because
//! an attacker uses a different name each time; a concurrency ceiling does not
//! bound guessing, because guesses one at a time are still unlimited.
//!//!
//! # Which half is the process's and which is the store's
//!
//! The concurrency ceiling bounds memory, and the memory is this **process's**:
//! a process told to verify twenty-four at once whose stores each counted to
//! twenty-four has been told nothing. `tessari-serve`'s door makes the same
//! argument for the connection ceiling, and it holds with more force for
//! memory. So the ceiling is one per process, here.
//!
//! The failure counts bound guessing at an **account**, and an account is a name
//! in one store's catalog, so they are the store's —
//! [`tessari_storage::Attempts`], shared by every handle to the store so a
//! reconnect does not reset them. They were one table per process until two
//! stores in one process were found delaying each other's users of one name
//! (Q-852).
//!
//! Neither change grows `Session::new`: the store is already in hand at every
//! sign-in site, and the ceiling needs nothing but the process.

use std::sync::{Arc, LazyLock};

use tessari_constants::MAX_SIGN_IN_VERIFICATIONS;
use tessari_serve::{Admitted, Admitting};
use tessari_storage::Store;

/// A sign-in budget shared with the other nodes of a cluster (ADR-0108 D5).
///
/// A guess sent to each of N nodes used to be N guesses' worth of allowance,
/// because each node counted alone. A clustered node asks the one table the
/// cluster keeps — the store line's leader's — before it hashes a password,
/// and tells it how the try went. A node that cannot ask in time counts on its
/// own table, which is what a partition costs and is said in the docs.
pub trait Budget: Send + Sync + std::fmt::Debug {
    /// Whether `name` may try now; `None` when the shared table could not be
    /// asked, so this node decides on its own.
    fn permit(&self, name: &str) -> Option<bool>;
    /// A try as `name` missed.
    fn failed(&self, name: &str);
    /// A try as `name` succeeded.
    fn succeeded(&self, name: &str);
}

/// What the store line's leader answers a peer asking for `name` (ADR-0108
/// D5): `store`'s own table, the cluster's while this node leads.
#[must_use]
pub fn permit_shared(store: &Store, name: &str) -> bool {
    store.attempts().permit(name)
}

/// A peer reports that a try as `name` missed there.
pub fn failed_shared(store: &Store, name: &str) {
    store.attempts().failed(name);
}

/// A peer reports that a try as `name` succeeded there.
pub fn succeeded_shared(store: &Store, name: &str) {
    store.attempts().succeeded(name);
}

/// Take one of this process's places to run a verification in, or `None` when
/// they are all taken.
///
/// The place is held for as long as the returned guard lives and comes back when
/// it is dropped, including when the thread holding it panics — the same
/// property, for the same reason, that the connection door has.
pub(crate) fn verifying() -> Option<Admitted> {
    static VERIFYING: LazyLock<Arc<Admitting>> =
        LazyLock::new(|| Admitting::to(MAX_SIGN_IN_VERIFICATIONS));
    VERIFYING.admit()
}

#[cfg(test)]
mod tests {
    use tessari_constants::{MAX_SIGN_IN_VERIFICATIONS, PASSWORD_HASH_MEMORY_KIB};

    #[test]
    fn the_ceiling_bounds_the_memory_it_was_chosen_to_bound() {
        // The assertion that would have caught the incident this module's
        // history is written around: the ceiling stood at a hundred thousand
        // instead of inside the budget, and nothing said so. The door test below would have
        // passed — it checks that the door is the size of the constant, not that
        // the constant is a bound.
        //
        // So the property asserted here is the *reason* for the number rather
        // than the number: the memory those verifications may hold at once. That
        // survives someone raising the ceiling for a real reason, and does not
        // survive raising it by accident.
        let held_kib = u64::try_from(MAX_SIGN_IN_VERIFICATIONS)
            .unwrap_or(u64::MAX)
            .saturating_mul(u64::from(PASSWORD_HASH_MEMORY_KIB));
        assert!(
            held_kib <= 512 * 1024,
            "{MAX_SIGN_IN_VERIFICATIONS} concurrent verifications at \
             {PASSWORD_HASH_MEMORY_KIB} KiB each is {held_kib} KiB held at the \
             peak, which is not a bound on a serving node's memory"
        );
    }

    #[test]
    fn the_process_hands_out_exactly_the_ceiling_and_takes_it_back() {
        // The concurrency ceiling, tested by **taking** the places rather than
        // by racing for them.
        //
        // The first version of this raced: thirty-two threads signing in at
        // once, asserting that some were refused. It cost the machine it ran on
        // eighteen gigabytes and never finished, because a mis-set ceiling made
        // every one of them hash at the same time and Argon2 is memory-hard on
        // purpose. A test that provokes the attack in order to prove the guard
        // against it is a test that runs the attack whenever the guard is
        // broken — which is precisely when it must not.
        //
        // So this holds the places directly. It spawns nothing, hashes nothing,
        // and cannot be slow: what it asserts is that the process has one door
        // of exactly the declared size and that a place comes back when it is
        // let go. No other test in this library signs in, so nothing is
        // perturbed by holding them for the length of a function.
        let held: Vec<_> = (0..MAX_SIGN_IN_VERIFICATIONS)
            .filter_map(|_| super::verifying())
            .collect();
        assert_eq!(
            held.len(),
            MAX_SIGN_IN_VERIFICATIONS,
            "the process door is smaller than the ceiling it was built with"
        );
        assert!(
            super::verifying().is_none(),
            "the door admitted one past its ceiling"
        );
        drop(held);
        assert!(
            super::verifying().is_some(),
            "a place that was let go never came back, so the door closes by one \
             per verification until it is shut"
        );
    }
}
