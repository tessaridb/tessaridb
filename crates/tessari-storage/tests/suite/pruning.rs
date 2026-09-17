//! Pruning the log, and everything that has to know it happened.

#![allow(clippy::panic, clippy::unwrap_used)]

use std::sync::Arc;

use tessari_encoding::{LogId, encode_payload};
use tessari_kv::{KvBackend, MemoryBackend};
use tessari_storage::{Catalog, Error, RecordAddress, Store, TableShape};
use tessari_types::{RecordId, Sequence, Value};

use crate::{FIXTURE_HOME, fixture_log};

fn store() -> Store {
    let backend = Arc::new(MemoryBackend::new()) as Arc<dyn KvBackend>;
    Store::open(backend).unwrap()
}

/// A namespace, database and table, committed, and the address to write at.
fn tree(store: &Store) -> RecordAddress {
    let mut transaction = store.begin().unwrap();
    let mut catalog = Catalog::new(&mut transaction);
    let namespace = catalog.create_namespace("prod").unwrap();
    let database = catalog.create_database(namespace.id, "orders").unwrap();
    let table = catalog
        .create_table(namespace.id, database.id, "users", TableShape::default())
        .unwrap();
    transaction.commit().unwrap();
    RecordAddress {
        namespace: namespace.id,
        database: database.id,
        table: table.id,
        id: RecordId::Int(1),
    }
}

/// Write one version of a record into [`FIXTURE_HOME`].
fn write(store: &Store, at: &RecordAddress, name: &str) {
    let mut transaction = store.begin().unwrap();
    let mut object = std::collections::BTreeMap::new();
    object.insert("name".to_owned(), Value::String(name.to_owned()));
    transaction.put(
        at.clone(),
        encode_payload(&Value::Object(object)).into_bytes(),
    );
    transaction.commit().unwrap();
}

/// A store holding `records` writes in its fixture log, and that log.
fn logged(records: usize) -> (Store, RecordAddress, LogId) {
    let store = store();
    let at = tree(&store);
    for index in 0..records {
        write(&store, &at, &format!("name-{index}"));
    }
    let log = fixture_log(&store);
    (store, at, log)
}

#[test]
fn a_log_nobody_has_pruned_begins_at_zero_and_reads_from_anywhere() {
    let (store, _, log) = logged(3);
    assert_eq!(
        store.log_start(log).unwrap(),
        Sequence::ZERO,
        "absence means the log is WHOLE, not that it is empty"
    );
    assert_eq!(
        store.log_records(log, Sequence::ZERO, 64).unwrap().len(),
        3,
        "and a read from the beginning is answered"
    );
}

#[test]
fn a_prune_removes_the_records_below_it_and_records_where_the_log_now_begins() {
    let (store, _, log) = logged(5);
    let tail = store.committed_tail(log).unwrap();
    assert_eq!(tail, Sequence::new(5));

    let pruned = store.prune_log(log, Sequence::new(2)).unwrap();
    assert_eq!(pruned.records, 2);
    assert_eq!(pruned.start, Sequence::new(3));
    assert_eq!(store.log_start(log).unwrap(), Sequence::new(3));

    let held = store.log_records(log, Sequence::new(3), 64).unwrap();
    assert_eq!(
        held.iter().map(|(at, _)| at.get()).collect::<Vec<_>>(),
        vec![3, 4, 5],
        "everything above the start survives, in order"
    );
}

#[test]
fn a_read_below_the_start_is_refused_in_words_that_name_the_repair() {
    let (store, _, log) = logged(5);
    store.prune_log(log, Sequence::new(2)).unwrap();

    let refused = store
        .log_records(log, Sequence::new(1), 64)
        .expect_err("a read below the start must not be answered");
    let Error::BelowLogStart { asked, start } = refused else {
        panic!("a pruned span must refuse as itself: {refused:?}");
    };
    assert_eq!((asked, start), (1, 3));

    let said = tessari_storage::Error::BelowLogStart { asked, start }.to_string();
    assert!(
        said.contains("fresh copy of the state"),
        "a reader below the start cannot catch up, and the refusal has to say so \
         rather than leave it to be discovered: {said}"
    );
}

/// The failure this refusal exists to prevent, stated as its own case.
///
/// A short answer is how this protocol says *you are level*. If a pruned span
/// were answered short instead of refused, a follower at position 1 would be
/// handed records 3, 4 and 5 — or nothing at all — and would conclude it was
/// caught up while two records it will never see are missing. Nothing anywhere
/// would be in an error state, which is why this is a refusal and not a log line.
#[test]
fn a_pruned_span_is_never_answered_as_a_short_read() {
    let (store, _, log) = logged(5);
    store.prune_log(log, Sequence::new(2)).unwrap();
    assert!(
        store.log_records(log, Sequence::ZERO, 64).is_err(),
        "a replay from the beginning of a pruned log must refuse"
    );
    assert!(
        store.log_records(log, Sequence::new(3), 64).is_ok(),
        "and the first surviving position must still be readable"
    );
}

#[test]
fn the_last_record_survives_however_much_is_asked_for() {
    let (store, _, log) = logged(4);
    let pruned = store.prune_log(log, Sequence::new(9_999)).unwrap();
    assert_eq!(
        pruned.start,
        Sequence::new(4),
        "the tail record is kept, because a level follower is served by reading \
         the record BEFORE the position it asks for"
    );
    assert_eq!(
        store.log_records(log, Sequence::new(4), 64).unwrap().len(),
        1
    );
}

#[test]
fn pruning_twice_to_the_same_point_changes_nothing() {
    let (store, _, log) = logged(5);
    let first = store.prune_log(log, Sequence::new(2)).unwrap();
    let second = store.prune_log(log, Sequence::new(2)).unwrap();
    assert_eq!(first.start, second.start);
    assert_eq!(
        second.records, 0,
        "a retried prune is idempotent, because a prune interrupted by a crash \
         is retried by definition"
    );
    assert_eq!(store.log_start(log).unwrap(), Sequence::new(3));
}

#[test]
fn a_prune_below_where_the_log_already_begins_does_not_move_it_backwards() {
    let (store, _, log) = logged(5);
    store.prune_log(log, Sequence::new(3)).unwrap();
    let back = store.prune_log(log, Sequence::new(1)).unwrap();
    assert_eq!(
        back.start,
        Sequence::new(4),
        "a start only ever rises; records that are gone do not come back"
    );
}

/// `INFO FOR HISTORY OF` claimed completeness from the walk alone, and pruning
/// is what made that claim false.
#[test]
fn a_history_read_over_a_pruned_log_no_longer_claims_to_be_complete() {
    let (store, at, log) = logged(4);
    let subject = tessari_storage::Subject::new(at.namespace, at.database, at.table, at.id.clone());
    let whole = store.history_of(log, &subject, 64).unwrap();
    assert!(
        whole.complete,
        "an unpruned log's history is complete when the walk reaches its start"
    );

    store.prune_log(log, Sequence::new(2)).unwrap();
    let after = store.history_of(log, &subject, 64).unwrap();
    assert!(
        !after.complete,
        "the walk still ends early, and now it ends at the horizon rather than \
         at the beginning — reporting that as complete would present a truncated \
         history as the whole of one"
    );
}

/// Pruning one log does not touch the log beside it.
///
/// A log is a home AND a writer, and the key carries both — so a range that got
/// its bounds wrong by a field would delete another range's history and report
/// success. Nothing downstream could tell that from a range that was empty.
#[test]
fn a_prune_is_bounded_to_the_log_it_names() {
    let (store, _, log) = logged(4);
    let store_log = store.own_log(tessari_types::Reach::Store).unwrap();
    let beside = store
        .log_records(store_log, Sequence::ZERO, 64)
        .unwrap()
        .len();
    assert!(
        beside > 0,
        "the namespace and database definitions are filed at the store's home"
    );
    assert_ne!(log, store_log);

    store.prune_log(log, Sequence::new(3)).unwrap();

    assert_eq!(
        store
            .log_records(store_log, Sequence::ZERO, 64)
            .unwrap()
            .len(),
        beside,
        "the store's own log is untouched, and its start never moved"
    );
    assert_eq!(store.log_start(store_log).unwrap(), Sequence::ZERO);
    assert_eq!(FIXTURE_HOME, log.home);
}

#[test]
fn a_store_nobody_configured_keeps_the_whole_log() {
    let (store, _, log) = logged(5);
    assert_eq!(
        store.log_retention().unwrap(),
        None,
        "unbounded is the default, because the default for an irreversible \
         operation has to be the one that changes nothing"
    );
    assert!(
        store.trim_logs().unwrap().is_none(),
        "and *nobody asked for this* is a different answer from *there was \
         nothing to do*"
    );
    assert_eq!(store.log_start(log).unwrap(), Sequence::ZERO);
}

#[test]
fn a_retained_count_is_what_the_log_is_trimmed_to() {
    let (store, _, log) = logged(10);
    store.set_log_retention(Some(Sequence::new(4))).unwrap();
    assert_eq!(store.log_retention().unwrap(), Some(Sequence::new(4)));

    let trimmed = store.trim_logs().unwrap().expect("retention is set");
    assert!(trimmed.logs >= 1, "every log this node holds is looked at");
    assert_eq!(trimmed.records, 6);
    assert_eq!(
        store.log_start(log).unwrap(),
        Sequence::new(7),
        "ten records keeping four leaves seven through ten"
    );
    assert_eq!(
        store.log_records(log, Sequence::new(7), 64).unwrap().len(),
        4
    );
}

#[test]
fn trimming_again_at_the_same_tail_removes_nothing_more() {
    let (store, _, log) = logged(10);
    store.set_log_retention(Some(Sequence::new(4))).unwrap();
    store.trim_logs().unwrap();
    let again = store.trim_logs().unwrap().expect("retention is still set");
    assert_eq!(
        again.records, 0,
        "the cadence runs every few seconds, so a trim that found nothing has \
         to cost nothing and say nothing"
    );
    assert_eq!(store.log_start(log).unwrap(), Sequence::new(7));
}

#[test]
fn clearing_the_retention_stops_the_trimming_without_restoring_anything() {
    let (store, _, log) = logged(10);
    store.set_log_retention(Some(Sequence::new(4))).unwrap();
    store.trim_logs().unwrap();
    store.set_log_retention(None).unwrap();

    assert_eq!(store.log_retention().unwrap(), None);
    assert!(store.trim_logs().unwrap().is_none());
    assert_eq!(
        store.log_start(log).unwrap(),
        Sequence::new(7),
        "records that are gone do not come back, and the start goes on saying so"
    );
}

/// A window wider than the log leaves it exactly as it was.
///
/// The case an operator actually sets on day one — a large number against a
/// young store — and the one where an off-by-one would quietly eat the history
/// of a store that had nothing to spare.
#[test]
fn a_window_wider_than_the_log_removes_nothing() {
    let (store, _, log) = logged(5);
    store
        .set_log_retention(Some(Sequence::new(100_000)))
        .unwrap();
    let trimmed = store.trim_logs().unwrap().expect("retention is set");
    assert_eq!(trimmed.records, 0);
    assert_eq!(store.log_start(log).unwrap(), Sequence::ZERO);
    assert_eq!(
        store.log_records(log, Sequence::ZERO, 64).unwrap().len(),
        5,
        "and a read from the beginning is still answered"
    );
}
