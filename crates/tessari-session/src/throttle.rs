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
//!
//! # Why this is one thing per process and not one per session
//!
//! Both bound something the *process* owns: the memory is this process's, and
//! the guess rate is against this node. A counter held per session is defeated by
//! reconnecting, which is the whole point of the finding. `tessari-serve`'s door
//! already makes this argument for the connection ceiling — a process told to
//! hold four hundred connections whose two surfaces each counted to four hundred
//! has been told nothing — and it holds with more force for memory.
//!
//! It also means `Session::new` does not grow an argument. There are a hundred
//! and twelve places that call it, nearly all of them the embedded library, and
//! threading a network bound through every one of them to reach four sign-in
//! sites would be the largest change in this repository in service of the
//! smallest.
//!
//! # Why the failure counts live in a fixed table
//!
//! The obvious shape is a map from name to failure count, and a map keyed by a
//! name **an attacker supplies** is itself the unbounded growth this module
//! exists to prevent. Both repairs are worse than the disease: evicting the
//! oldest entry when the map fills lets an attacker reset a victim's counter on
//! demand, and refusing to add one lets an attacker fill the table with junk so
//! that the victim is never tracked at all.
//!
//! So: a fixed array of buckets, indexed by a hash of the name. The memory is
//! decided at compile time, there is no eviction policy to get wrong, and two
//! names landing in one bucket **share** a counter rather than evicting each
//! other — which is conservative, throttling sooner and never later. The cost of
//! sharing is that an attacker could delay a victim by failing against a
//! colliding name; they can already do that by failing against the victim's own
//! name, so it grants nothing new, and a delay that decays is not a lockout.
//!
//! Nothing here holds the name itself. A bucket index is a number, so a caller
//! sending a megabyte of name spends a megabyte once and leaves nothing behind.

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::sync::{Arc, LazyLock, Mutex, PoisonError};
use std::time::{Duration, Instant};

use tessari_constants::{
    FREE_SIGN_IN_FAILURES, MAX_SIGN_IN_VERIFICATIONS, SIGN_IN_BACKOFF_CEILING_MILLIS,
    SIGN_IN_BACKOFF_MILLIS,
};
use tessari_serve::{Admitted, Admitting};

/// How many failure counters this node keeps.
///
/// A power of two so the index is a mask, and far above the number of users any
/// store has, so ordinary use never shares a bucket. It is a bound on memory and
/// not a bound on identities: a store with more users than this still
/// authenticates all of them, and merely counts some of their failures together.
///
/// Not in `tessari-constants` for the reason wave 127's batch size is not
/// (ADR-0042): it is the shape of one data structure rather than a tunable of
/// the system, and nothing outside this file can act on it.
const BUCKETS: usize = 1024;

/// What one bucket remembers.
#[derive(Debug, Clone, Copy, Default)]
struct Bucket {
    /// Consecutive failures, cleared by a success.
    missed: u32,
    /// When attempts may resume, while that is in the future.
    until: Option<Instant>,
}

/// The failure counts, and the delay each one has earned.
#[derive(Debug)]
pub(crate) struct Attempts {
    buckets: Mutex<Box<[Bucket; BUCKETS]>>,
}

impl Attempts {
    /// A node nobody has failed against yet.
    pub(crate) fn new() -> Self {
        Self {
            buckets: Mutex::new(Box::new([Bucket::default(); BUCKETS])),
        }
    }

    /// Whether `name` may be tried right now.
    ///
    /// Asked **before** the store is read and before the hasher is reached, so a
    /// throttled attempt costs a lock and an index rather than a transaction and
    /// nineteen mebibytes. That ordering is the point of the whole module: a
    /// refusal that still paid for the verification would have bounded nothing.
    pub(crate) fn permit(&self, name: &str) -> bool {
        let mut buckets = self.buckets.lock().unwrap_or_else(PoisonError::into_inner);
        let bucket = &mut buckets[index_of(name)];
        match bucket.until {
            None => true,
            Some(waiting) if Instant::now() < waiting => false,
            // The wait is served. Attempts resume, and the count is deliberately
            // **not** cleared: waiting is what the count bought, so clearing it
            // here would make the delay a fixed one dressed up as a doubling
            // one, and a fixed delay is a fixed guess rate. Only a success
            // clears it.
            Some(_) => {
                bucket.until = None;
                true
            }
        }
    }

    /// Record that `name` did not match, and set what it now has to wait.
    pub(crate) fn failed(&self, name: &str) {
        let mut buckets = self.buckets.lock().unwrap_or_else(PoisonError::into_inner);
        let bucket = &mut buckets[index_of(name)];
        bucket.missed = bucket.missed.saturating_add(1);
        // The allowance is spent *by* the third miss, not after a fourth. Written
        // the other way round first, and the test above caught it: an off-by-one
        // here is one free guess per identity per backoff window, forever.
        if bucket.missed >= FREE_SIGN_IN_FAILURES {
            let over = bucket
                .missed
                .saturating_sub(FREE_SIGN_IN_FAILURES)
                .saturating_add(1);
            bucket.until = Some(Instant::now().checked_add(wait_after(over)).unwrap_or_else(
                // A clock far enough from the epoch that adding thirty seconds
                // overflows is not a clock this node can reason about, and the
                // safe reading of it is that the wait has been served.
                Instant::now,
            ));
        }
    }

    /// Record that `name` matched, and give back its allowance.
    ///
    /// A success clears the bucket rather than decrementing it, because the count
    /// measures *consecutive* misses and a match ends the run. It also means a
    /// person who mistyped twice and then got it right starts the next day with
    /// three tries again, rather than with the residue of a Tuesday.
    pub(crate) fn succeeded(&self, name: &str) {
        let mut buckets = self.buckets.lock().unwrap_or_else(PoisonError::into_inner);
        buckets[index_of(name)] = Bucket::default();
    }
}

/// How long an identity waits after `over` failures past the free allowance.
///
/// Doubles, and stops doubling at the ceiling. `over` is one for the first
/// throttled miss, so the first wait is the base and nothing waits for zero.
fn wait_after(over: u32) -> Duration {
    let doublings = over.saturating_sub(1).min(u32::BITS.saturating_sub(1));
    let millis = SIGN_IN_BACKOFF_MILLIS
        .saturating_mul(1_u64.checked_shl(doublings).unwrap_or(u64::MAX))
        .min(SIGN_IN_BACKOFF_CEILING_MILLIS);
    Duration::from_millis(millis)
}

/// Which bucket a name's failures are counted in.
///
/// `BUCKETS` is a power of two, so the low bits **are** the index: this is a
/// mask, with no remainder to divide and no cast that can narrow. Written as a
/// remainder first, and the linter was right to refuse it — the comment already
/// claimed a mask while the code did a division and a `u64`-to-`usize` cast.
fn index_of(name: &str) -> usize {
    let mut hasher = DefaultHasher::new();
    name.hash(&mut hasher);
    let mask = u64::try_from(BUCKETS.saturating_sub(1)).unwrap_or(u64::MAX);
    // Bucket zero is a real bucket, so a fallback that cannot be reached is
    // still a safe one rather than a hidden panic.
    usize::try_from(hasher.finish() & mask).unwrap_or(0)
}

/// This process's failure counts.
pub(crate) fn attempts() -> &'static Attempts {
    static ATTEMPTS: LazyLock<Attempts> = LazyLock::new(Attempts::new);
    &ATTEMPTS
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
    use super::{Attempts, BUCKETS, index_of, wait_after};
    use std::thread::sleep;
    use std::time::Duration;
    use tessari_constants::{
        FREE_SIGN_IN_FAILURES, MAX_SIGN_IN_VERIFICATIONS, PASSWORD_HASH_MEMORY_KIB,
        SIGN_IN_BACKOFF_CEILING_MILLIS, SIGN_IN_BACKOFF_MILLIS,
    };

    #[test]
    fn the_free_allowance_is_spent_before_anyone_waits() {
        let attempts = Attempts::new();
        for _ in 0..FREE_SIGN_IN_FAILURES {
            assert!(attempts.permit("root"), "a free attempt was refused");
            attempts.failed("root");
        }
        assert!(
            !attempts.permit("root"),
            "the attempt after the allowance was not made to wait"
        );
    }

    #[test]
    fn a_success_gives_the_allowance_back() {
        let attempts = Attempts::new();
        for _ in 0..FREE_SIGN_IN_FAILURES {
            attempts.failed("root");
        }
        assert!(!attempts.permit("root"));
        attempts.succeeded("root");
        // Not merely permitted again — the whole allowance is back, which is the
        // difference between clearing the bucket and clearing the deadline.
        for _ in 0..FREE_SIGN_IN_FAILURES {
            assert!(attempts.permit("root"));
            attempts.failed("root");
        }
        assert!(!attempts.permit("root"));
    }

    #[test]
    fn serving_the_wait_does_not_give_the_allowance_back() {
        // The property that makes the delay double rather than repeat. If
        // `permit` cleared the count when the wait expired, every further miss
        // would wait the base again and the throttle would be a fixed rate
        // limiter wearing a doubling one's clothes.
        let attempts = Attempts::new();
        for _ in 0..FREE_SIGN_IN_FAILURES {
            attempts.failed("root");
        }
        attempts.failed("root");
        assert!(!attempts.permit("root"));
        sleep(Duration::from_millis(SIGN_IN_BACKOFF_MILLIS * 2 + 50));
        assert!(attempts.permit("root"), "the wait was never served");
        attempts.failed("root");
        // One further miss, and the wait is longer than the base rather than
        // equal to it — which it would be if the count had been reset.
        assert!(!attempts.permit("root"));
        sleep(Duration::from_millis(SIGN_IN_BACKOFF_MILLIS + 20));
        assert!(
            !attempts.permit("root"),
            "the second wait was no longer than the first, so the count reset"
        );
    }

    #[test]
    fn one_identity_failing_does_not_delay_a_different_one() {
        let attempts = Attempts::new();
        for _ in 0..=FREE_SIGN_IN_FAILURES {
            attempts.failed("root");
        }
        assert!(!attempts.permit("root"));
        // Two names chosen because they land in different buckets, asserted
        // rather than assumed — a test that silently picked a collision would
        // pass while proving the opposite of its name.
        assert_ne!(index_of("root"), index_of("editor"));
        assert!(attempts.permit("editor"));
    }

    #[test]
    fn the_wait_doubles_and_then_stops_at_the_ceiling() {
        assert_eq!(wait_after(1), Duration::from_millis(SIGN_IN_BACKOFF_MILLIS));
        assert_eq!(
            wait_after(2),
            Duration::from_millis(SIGN_IN_BACKOFF_MILLIS * 2)
        );
        assert_eq!(
            wait_after(3),
            Duration::from_millis(SIGN_IN_BACKOFF_MILLIS * 4)
        );
        assert_eq!(
            wait_after(40),
            Duration::from_millis(SIGN_IN_BACKOFF_CEILING_MILLIS),
            "a long run of failures must not reach a wait measured in days"
        );
        // The shift is what would overflow, and it is asked for a number far
        // past the width of the thing being shifted. A wrong answer here is a
        // wait of zero, which is the throttle silently switched off.
        assert_eq!(
            wait_after(u32::MAX),
            Duration::from_millis(SIGN_IN_BACKOFF_CEILING_MILLIS)
        );
    }

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

    #[test]
    fn every_name_lands_inside_the_table() {
        for name in ["", "root", "a name with spaces", &"x".repeat(100_000)] {
            assert!(index_of(name) < BUCKETS, "{name} indexed outside the table");
        }
    }
}
