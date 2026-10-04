#![allow(clippy::unwrap_used)]

use tessari_types::{DatabaseId, Epoch, NamespaceId, Reach};

use super::*;

const WRITER: Writer = Writer::new([1; 16]);

fn log(home: Reach) -> LogId {
    LogId::new(home, WRITER)
}

fn database() -> LogId {
    log(Reach::Database(NamespaceId::new(1), DatabaseId::new(1)))
}

fn namespace() -> LogId {
    log(Reach::Namespace(NamespaceId::new(1)))
}

/// Records at positions 1.. carrying the given orders.
fn records(orders: &[u64]) -> Vec<(Sequence, LogRecord)> {
    orders
        .iter()
        .zip(1_u64..)
        .map(|(order, at)| {
            let mut record = LogRecord::at(Epoch::new(1), Vec::new());
            record.set_order(Sequence::new(*order));
            (Sequence::new(at), record)
        })
        .collect()
}

#[test]
fn records_of_two_logs_are_applied_interleaved_in_the_writers_order() {
    let finer = records(&[2, 4]);
    let coarser = records(&[3]);
    let pages = [
        Page {
            from: Sequence::new(1),
            previous: Epoch::ZERO,
            log: namespace(),
            records: &coarser,
            horizon: Horizon::Level(Sequence::new(10)),
        },
        Page {
            from: Sequence::new(1),
            previous: Epoch::ZERO,
            log: database(),
            records: &finer,
            horizon: Horizon::Level(Sequence::new(10)),
        },
    ];
    assert_eq!(in_writer_order(&pages), vec![(1, 0), (0, 0), (1, 1)]);
}

/// Records at positions 1.. carrying `(epoch, order)` pairs.
fn stamped(pairs: &[(u64, u64)]) -> Vec<(Sequence, LogRecord)> {
    pairs
        .iter()
        .zip(1_u64..)
        .map(|((epoch, order), at)| {
            let mut record = LogRecord::at(Epoch::new(*epoch), Vec::new());
            record.set_order(Sequence::new(*order));
            (Sequence::new(at), record)
        })
        .collect()
}

/// ADR-0107: a single-leader range's logs are continued by each leader in
/// turn, and each stamps its OWN counter — so the leader at epoch 2 may
/// stamp 5 where its predecessor at epoch 1 had reached 50. Every commit of
/// an earlier leadership precedes every commit of a later one, so the order
/// is the pair, and a level answer from the epoch-2 leader proves all of
/// epoch 1 and epoch 2 up to its own counter.
#[test]
fn a_later_leadership_follows_an_earlier_one_whatever_its_counter_says() {
    let finer = stamped(&[(1, 50), (2, 5)]);
    let coarser = stamped(&[(1, 48), (2, 6)]);
    let pages = [
        Page {
            from: Sequence::new(1),
            previous: Epoch::ZERO,
            log: namespace(),
            records: &coarser,
            horizon: Horizon::LevelAt(Epoch::new(2), Sequence::new(6)),
        },
        Page {
            from: Sequence::new(1),
            previous: Epoch::ZERO,
            log: database(),
            records: &finer,
            horizon: Horizon::LevelAt(Epoch::new(2), Sequence::new(6)),
        },
    ];
    assert_eq!(
        in_writer_order(&pages),
        vec![(0, 0), (1, 0), (1, 1), (0, 1)],
        "epoch 1's 48 and 50, then epoch 2's 5 and 6"
    );
}

/// The case a log at a time gets wrong, and the case the horizon exists
/// for: the namespace page was fetched AFTER the database page, and holds a
/// commit (order 7) that may follow one written to the database log after
/// the database page was read (order 6, not in the page). The database
/// page's level proves only up to 5, so 7 waits.
#[test]
fn a_record_past_what_every_page_proves_waits_for_the_next_round() {
    let finer = records(&[3]);
    let coarser = records(&[7]);
    let pages = [
        Page {
            from: Sequence::new(1),
            previous: Epoch::ZERO,
            log: database(),
            records: &finer,
            horizon: Horizon::Level(Sequence::new(5)),
        },
        Page {
            from: Sequence::new(1),
            previous: Epoch::ZERO,
            log: namespace(),
            records: &coarser,
            horizon: Horizon::Level(Sequence::new(9)),
        },
    ];
    assert_eq!(in_writer_order(&pages), vec![(0, 0)]);
}

#[test]
fn a_full_page_proves_its_log_only_up_to_its_last_record() {
    let full = records(&[2, 8]);
    let level = records(&[5, 9]);
    let pages = [
        Page {
            from: Sequence::new(1),
            previous: Epoch::ZERO,
            log: database(),
            records: &full,
            horizon: Horizon::Full,
        },
        Page {
            from: Sequence::new(1),
            previous: Epoch::ZERO,
            log: namespace(),
            records: &level,
            horizon: Horizon::Level(Sequence::new(10)),
        },
    ];
    assert_eq!(in_writer_order(&pages), vec![(0, 0), (1, 0), (0, 1)]);
}

/// Progress: the page that bounds the round is always applied whole when
/// it is full, so a round never stalls on a log that holds records.
#[test]
fn the_page_that_bounds_a_round_is_applied_whole() {
    let full = records(&[4, 6]);
    let later = records(&[7, 8]);
    let pages = [
        Page {
            from: Sequence::new(1),
            previous: Epoch::ZERO,
            log: database(),
            records: &full,
            horizon: Horizon::Full,
        },
        Page {
            from: Sequence::new(1),
            previous: Epoch::ZERO,
            log: namespace(),
            records: &later,
            horizon: Horizon::Full,
        },
    ];
    assert_eq!(in_writer_order(&pages), vec![(0, 0), (0, 1)]);
}

#[test]
fn records_without_an_order_keep_collection_order_as_before() {
    let unordered = |count: u64| -> Vec<(Sequence, LogRecord)> {
        (1..=count)
            .map(|at| (Sequence::new(at), LogRecord::at(Epoch::new(1), Vec::new())))
            .collect()
    };
    let first = unordered(2);
    let second = unordered(1);
    let pages = [
        Page {
            from: Sequence::new(1),
            previous: Epoch::ZERO,
            log: namespace(),
            records: &first,
            horizon: Horizon::Unstated,
        },
        Page {
            from: Sequence::new(1),
            previous: Epoch::ZERO,
            log: database(),
            records: &second,
            horizon: Horizon::Unstated,
        },
    ];
    assert_eq!(in_writer_order(&pages), vec![(0, 0), (0, 1), (1, 0)]);
}

#[test]
fn two_writers_are_bounded_each_by_their_own_pages() {
    let other = Writer::new([2; 16]);
    let mine = records(&[3, 50]);
    let theirs = records(&[1]);
    let pages = [
        Page {
            from: Sequence::new(1),
            previous: Epoch::ZERO,
            log: database(),
            records: &mine,
            horizon: Horizon::Level(Sequence::new(60)),
        },
        Page {
            from: Sequence::new(1),
            previous: Epoch::ZERO,
            log: LogId::new(Reach::Store, other),
            records: &theirs,
            horizon: Horizon::Level(Sequence::new(1)),
        },
    ];
    // The other writer's small bound does not hold this writer's 50 back.
    assert_eq!(in_writer_order(&pages), vec![(1, 0), (0, 0), (0, 1)]);
}
