use super::{
    COMMIT_BACKOFF_CEILING, COMMIT_BACKOFF_STEP, MAX_COMMIT_ATTEMPTS, jitter, waiting_for,
    window_for,
};

#[test]
fn the_wait_doubles_until_it_reaches_the_ceiling_and_then_stops() {
    // The property the doubling exists for: the window a loser is spread
    // across widens with the contention rather than being guessed in
    // advance. Asserted on the *bound*, by handing in a jitter that always
    // lands at the top of the window, because the wait itself is random by
    // design and a test that asserted an exact wait would be asserting the
    // jitter away.
    let mut previous = 0;
    let mut flattened = false;
    for attempt in 1..=MAX_COMMIT_ATTEMPTS {
        let now = window_for(attempt);
        assert!(
            now >= previous,
            "attempt {attempt} has a narrower window than the one before it: {now} < {previous}"
        );
        assert!(
            now <= COMMIT_BACKOFF_CEILING,
            "attempt {attempt} opens a window of {now}, past the ceiling"
        );
        if now == previous && attempt > 1 {
            flattened = true;
        }
        previous = now;
    }
    assert_eq!(
        window_for(MAX_COMMIT_ATTEMPTS.saturating_add(4)),
        COMMIT_BACKOFF_CEILING,
        "the doubling never reaches the ceiling, so it is not bounded by it"
    );
    // Not a fact about the constants so much as a check that they still say
    // what the doc comment claims: the budget is spent before the ceiling
    // makes the last attempts indistinguishable. If a future value of
    // MAX_COMMIT_ATTEMPTS flattens the tail, this says so.
    assert!(
        !flattened,
        "the window flattens before the budget is spent, so the last attempts no longer widen"
    );
}

#[test]
fn the_wait_stays_inside_its_window_whatever_the_jitter_is() {
    for attempt in 0..64_u32 {
        for offered in [0, 1, 7, 4_999, u64::MAX / 3, u64::MAX] {
            let waited = waiting_for(attempt, offered).as_micros();
            assert!(
                waited < u128::from(window_for(attempt).max(1)),
                "attempt {attempt} with jitter {offered} waited {waited}, \
                     outside its own window of {}",
                window_for(attempt)
            );
        }
    }
}

#[test]
fn a_first_loss_still_waits_rather_than_re_racing_into_the_same_instant() {
    // The whole point, reduced to one row. A jitter that reduces to zero is
    // allowed — full jitter includes zero — so the assertion is on the
    // window rather than on every draw: attempt one must have room to wait.
    assert!(
        window_for(1) >= COMMIT_BACKOFF_STEP,
        "the first retry has no window to spread into"
    );
}

#[test]
fn two_threads_do_not_compute_the_same_wait() {
    // The jitter is load-bearing rather than decorative: writers that lose
    // together and wait the same amount arrive together, which rebuilds the
    // collision the wait exists to break. Drawn many times per thread
    // because a single pair could coincide by chance, and compared as
    // sequences because two threads sharing a seed would agree on all of it.
    let mine: Vec<u64> = (0..32).map(|_| jitter()).collect();
    let theirs = std::thread::spawn(|| (0..32).map(|_| jitter()).collect::<Vec<u64>>())
        .join()
        .expect("the other thread drew its own");
    assert_ne!(mine, theirs, "two threads drew the same sequence of waits");
}

#[test]
fn one_thread_does_not_draw_one_number_forever() {
    // Guards the seeding: a state left at zero would xorshift to zero for
    // ever, and every retry would wait exactly nothing — which is the
    // behaviour being removed, arriving back through the jitter.
    let drawn: std::collections::BTreeSet<u64> = (0..32).map(|_| jitter()).collect();
    assert!(drawn.len() > 1, "the jitter is a constant: {drawn:?}");
    assert!(
        !drawn.contains(&0),
        "the jitter state reached zero and stuck"
    );
}
