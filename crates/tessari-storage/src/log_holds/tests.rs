use std::collections::BTreeSet;

use super::LogHolds;

#[test]
fn holds_taken_at_once_from_many_threads_never_share_an_id() {
    // An id is what `release` finds a hold by, so two holds sharing one would
    // let the first copy to finish release the other's positions as well — and
    // the log would be pruned under a follower still collecting from it.
    const THREADS: usize = 8;
    const EACH: usize = 1_000;
    let holds = LogHolds::shared();
    let taken: Vec<Vec<u64>> = std::thread::scope(|scope| {
        let workers: Vec<_> = (0..THREADS)
            .map(|_| scope.spawn(|| (0..EACH).map(|_| holds.next_id()).collect::<Vec<_>>()))
            .collect();
        workers
            .into_iter()
            .map(|worker| worker.join().expect("a worker thread"))
            .collect()
    });
    let distinct: BTreeSet<u64> = taken.iter().flatten().copied().collect();
    assert_eq!(
        distinct.len(),
        THREADS * EACH,
        "two holds were given one id"
    );
    assert!(!distinct.contains(&0), "an id of zero was given");
}
