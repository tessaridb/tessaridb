// The panic below is the subject of a test, not a hazard in one: a place
// must come back when the thread holding it dies.
#![allow(clippy::panic, clippy::expect_used)]

use super::*;

#[test]
fn the_door_admits_up_to_its_limit_and_then_refuses() {
    let door = Admitting::to(2);
    let first = door.admit().expect("the first is under the limit");
    let second = door.admit().expect("the second reaches it");
    assert_eq!(door.open(), 2);
    assert!(door.admit().is_none(), "the third is over it");
    assert_eq!(door.refused(), 1);

    // Refusing must not be mistaken for a leak: the two that were admitted
    // are still held, and the count still says two.
    assert_eq!(door.open(), 2);
    drop(first);
    assert_eq!(door.open(), 1);
    assert!(door.admit().is_some(), "a place freed is a place given");
    drop(second);
}

#[test]
fn a_place_is_returned_even_when_the_thread_holding_it_panics() {
    // The property the `Drop` impl exists for. Without it a panicking
    // connection thread closes the door by one, permanently, and the node
    // degrades in a way nothing points at the panic that caused it.
    let door = Admitting::to(1);
    let held = Arc::clone(&door);
    let panicked = std::thread::spawn(move || {
        let _place = held.admit().expect("the only place");
        panic!("a connection that went wrong");
    })
    .join();
    assert!(panicked.is_err(), "the thread should have panicked");
    assert_eq!(door.open(), 0, "the place should have come back");
    assert!(door.admit().is_some());
}

#[test]
fn every_thread_racing_for_the_last_places_sees_one_ceiling() {
    // The compare-exchange loop's reason to exist. A read-then-add would let
    // two threads both see `limit - 1` and both admit, which is the bug an
    // atomic counter without a loop actually has.
    let door = Admitting::to(50);
    let taken: Vec<_> = (0..8)
        .map(|_| {
            let held = Arc::clone(&door);
            std::thread::spawn(move || (0..20).filter_map(|_| held.admit()).collect::<Vec<_>>())
        })
        .map(|racing| racing.join().expect("a racing thread"))
        .collect();
    let admitted: usize = taken.iter().map(Vec::len).sum();
    assert_eq!(admitted, 50, "never more than the ceiling, and never fewer");
    assert_eq!(door.open(), 50);
    assert_eq!(door.refused(), 110);
}

#[test]
fn a_guard_that_is_dropped_is_no_longer_in_flight() {
    let stopping = Stopping::new();
    assert_eq!(stopping.requests(), 0);
    let one = stopping.busy();
    let two = stopping.busy();
    assert_eq!(stopping.requests(), 2);
    drop(one);
    assert_eq!(stopping.requests(), 1);
    drop(two);
    assert_eq!(stopping.requests(), 0);
}

#[test]
fn a_connection_that_becomes_a_feed_leaves_the_request_count() {
    // The property stage 2 depends on: once a connection is a feed, waiting
    // for requests can still succeed. Counted together it never could.
    let stopping = Stopping::new();
    let mut held = stopping.busy();
    assert_eq!((stopping.requests(), stopping.feeds()), (1, 0));
    held.became_a_feed();
    assert_eq!((stopping.requests(), stopping.feeds()), (0, 1));
    // Idempotent, because a caller that says it twice has not opened a
    // second subscription.
    held.became_a_feed();
    assert_eq!((stopping.requests(), stopping.feeds()), (0, 1));
    drop(held);
    assert_eq!((stopping.requests(), stopping.feeds()), (0, 0));
}

#[tokio::test(start_paused = true)]
async fn a_drain_waits_for_a_request_and_not_for_a_feed() {
    let stopping = Stopping::new();
    let mut feed = stopping.busy();
    feed.became_a_feed();
    // A feed is open, and the drain still finishes — which is the whole
    // reason the two counts are apart. Waiting on both, this would time out.
    assert_eq!(
        stopping.drain(Duration::from_secs(5)).await,
        Drained::Finished,
        "the drain waited for a subscription, which never ends on its own"
    );

    let working = stopping.busy();
    let patience = Duration::from_millis(30);
    assert_eq!(
        stopping.drain(patience).await,
        Drained::Deadline { left: 1 },
        "the drain did not wait for a request that never finished"
    );
    drop(working);
    assert_eq!(stopping.drain(patience).await, Drained::Finished);
}

#[test]
fn a_node_stops_being_ready_before_it_stops_accepting() {
    // The window the readiness route is reached in. If these were one flag
    // the port would close at the instant the answer changed, and nothing
    // outside the process could ever see the 503.
    let stopping = Stopping::new();
    assert!(stopping.ready());
    assert!(!stopping.asked());

    stopping.leaving();
    assert!(
        !stopping.ready(),
        "a leaving node still called itself ready"
    );
    assert!(
        !stopping.asked(),
        "stage 0 closed the port, so the readiness answer it just changed \
             cannot be reached by anything"
    );

    stopping.refuse_new();
    assert!(!stopping.ready());
    assert!(stopping.asked());
}

#[test]
fn refusing_connections_implies_no_longer_being_ready() {
    // A caller that skips stage 0 must not leave a node refusing new
    // connections while telling whatever can still reach it that it is
    // ready to take them.
    let stopping = Stopping::new();
    stopping.refuse_new();
    assert!(!stopping.ready());
}

#[test]
fn refusals_are_counted_among_answers_and_not_beside_them() {
    // `answers` includes refusals, so a dashboard can show a rate of one
    // against the other without a third number to keep consistent. Counted
    // beside each other instead, "how many requests did this node answer"
    // would need an addition that somebody eventually gets wrong.
    let stopping = Stopping::new();
    assert_eq!((stopping.answers(), stopping.refusals()), (0, 0));
    stopping.answered(false);
    stopping.answered(true);
    stopping.answered(false);
    assert_eq!(
        (stopping.answers(), stopping.refusals()),
        (3, 1),
        "a refusal was not counted as an answer"
    );
}

#[test]
fn a_redirect_is_counted_by_whether_the_caller_may_remember_it() {
    let stopping = Stopping::new();
    stopping.redirected(true);
    stopping.redirected(false);
    stopping.redirected(false);
    assert_eq!(stopping.redirects(), (1, 2));
}

#[test]
fn a_census_reports_every_surface_and_the_process_that_holds_them() {
    // The property a metrics route depends on and one surface cannot have:
    // it describes the process, so it must see counters it did not create.
    let wire = Stopping::new();
    let http = Stopping::new();
    wire.answered(false);

    let mut census = Census::since(Instant::now());
    census.counting("wire", Arc::clone(&wire));
    census.counting("http", Arc::clone(&http));

    let seen: Vec<_> = census
        .surfaces()
        .map(|(name, counted)| (name, counted.answers()))
        .collect();
    assert_eq!(seen, vec![("wire", 1), ("http", 0)]);
}

#[test]
fn stopping_is_asked_for_once_and_stays_asked() {
    let stopping = Stopping::new();
    assert!(!stopping.asked());
    stopping.refuse_new();
    assert!(stopping.asked());
    stopping.refuse_new();
    assert!(stopping.asked());
}
