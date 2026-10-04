use super::*;

#[test]
fn a_delayed_cadence_runs_once_however_many_periods_it_missed() {
    let period = Duration::from_secs(10);
    let ran_at = Instant::now();
    let late = ran_at
        .checked_add(Duration::from_secs(35))
        .expect("an instant 35s from now");
    assert_eq!(
        due_in(period, ran_at, late),
        Duration::ZERO,
        "a pass that overran by three periods asked for more than one catch-up"
    );
}

#[test]
fn a_cadence_that_is_early_waits_out_the_remainder() {
    let period = Duration::from_secs(10);
    let ran_at = Instant::now();
    let soon = ran_at
        .checked_add(Duration::from_secs(3))
        .expect("an instant 3s from now");
    assert_eq!(due_in(period, ran_at, soon), Duration::from_secs(7));
}

#[tokio::test(start_paused = true)]
async fn a_cadence_runs_no_pass_once_the_node_is_asked_to_stop() {
    let stop = CancellationToken::new();
    stop.cancel();
    let passes = Arc::new(AtomicUsize::new(0));
    let counting = Arc::clone(&passes);
    every(Duration::ZERO, &stop, move |_| {
        counting.fetch_add(1, Ordering::Relaxed);
    })
    .await;
    assert_eq!(
        passes.load(Ordering::Relaxed),
        0,
        "a node already stopping still ran a cadence pass"
    );
}

#[tokio::test(start_paused = true)]
async fn a_cadence_keeps_its_state_between_passes_and_a_stop_ends_its_wait() {
    let stop = CancellationToken::new();
    let passes = Arc::new(AtomicUsize::new(0));
    let counting = Arc::clone(&passes);
    let stopping = stop.clone();
    // State the closure owns, carried from one pass to the next across the
    // hop to the blocking pool and back.
    let mut rounds = 0_usize;
    let cadence = tokio::spawn(async move {
        every(Duration::from_secs(3600), &stopping, move |_| {
            rounds = rounds.saturating_add(1);
            counting.store(rounds, Ordering::Relaxed);
        })
        .await;
    });
    // The clock is paused and moves only when every task is waiting, so
    // each of these sleeps lets exactly the cadence's own wait run out.
    while passes.load(Ordering::Relaxed) < 3 {
        tokio::time::sleep(Duration::from_secs(3600)).await;
    }
    // Half a period on, so the stop lands in the middle of a wait rather than
    // at its end: the stop has to end that wait itself.
    tokio::time::sleep(Duration::from_secs(1800)).await;
    stop.cancel();
    tokio::time::timeout(Duration::from_secs(1), cadence)
        .await
        .expect("a stop did not end the cadence's wait")
        .expect("the cadence task");
    assert!(passes.load(Ordering::Relaxed) >= 3);
}

#[tokio::test]
async fn a_pass_that_panics_is_raised_on_the_cadence_task() {
    let stop = CancellationToken::new();
    let stopping = stop.clone();
    let cadence = tokio::spawn(async move {
        every(Duration::ZERO, &stopping, |_| {
            std::panic::resume_unwind(Box::new("a defect in a pass"));
        })
        .await;
    });
    let ended = cadence.await.expect_err("the panic did not reach the task");
    assert!(ended.is_panic());
}
