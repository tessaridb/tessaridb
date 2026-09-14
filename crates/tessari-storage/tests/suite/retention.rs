//! The retention floor, and the leak that would pin it forever.

#![allow(clippy::panic, clippy::unwrap_used)]

use std::sync::Arc;

use tessari_encoding::encode_payload;
use tessari_kv::{KvBackend, MemoryBackend};
use tessari_storage::{Catalog, RecordAddress, Store, TableShape};
use tessari_types::{RecordId, Sequence, Value};

fn store() -> Store {
    let backend = Arc::new(MemoryBackend::new()) as Arc<dyn KvBackend>;
    Store::open(backend).unwrap()
}

/// A namespace, database and table, committed.
fn tree(store: &Store) -> (u32, u32, u32) {
    let mut transaction = store.begin().unwrap();
    let mut catalog = Catalog::new(&mut transaction);
    let namespace = catalog.create_namespace("prod").unwrap();
    let database = catalog.create_database(namespace.id, "orders").unwrap();
    let table = catalog
        .create_table(namespace.id, database.id, "users", TableShape::default())
        .unwrap();
    let ids = (namespace.id.get(), database.id.get(), table.id.get());
    transaction.commit().unwrap();
    ids
}

/// Write one version of a record and return the **version** it landed at.
///
/// Not the log position `commit` returns. Every caller in this file uses the
/// answer as a snapshot coordinate — it is compared against a retention or
/// reclaim floor, or handed to `begin_at` — and those are versions. The two
/// numbers agree while one leader decides every write, so a helper returning
/// the position would keep passing and would stop meaning what its callers
/// read it as (Q-614).
fn write(store: &Store, at: &RecordAddress, name: &str) -> Sequence {
    let mut transaction = store.begin().unwrap();
    let mut object = std::collections::BTreeMap::new();
    object.insert("name".to_owned(), Value::String(name.to_owned()));
    transaction.put(
        at.clone(),
        encode_payload(&Value::Object(object)).into_bytes(),
    );
    transaction.commit().unwrap();
    store.committed_version().unwrap()
}

#[test]
fn the_floor_is_the_committed_version_when_nothing_is_being_read() {
    let store = store();
    let _ = tree(&store);
    assert_eq!(store.live_snapshots(), 0);
    assert_eq!(
        store.retention_floor().unwrap(),
        store.committed_version().unwrap()
    );
    assert_eq!(store.oldest_snapshot_age(), None);
}

#[test]
fn a_live_reader_holds_the_floor_where_it_began() {
    let store = store();
    let (namespace, database, table) = tree(&store);
    let at = RecordAddress::new(
        tessari_types::NamespaceId::new(namespace),
        tessari_types::DatabaseId::new(database),
        tessari_types::TableId::new(table),
        RecordId::Int(1),
    );

    write(&store, &at, "ada");
    let reader = store.begin().unwrap();
    let held = reader.snapshot();

    // The store moves on while the reader is still working.
    write(&store, &at, "grace");
    write(&store, &at, "hopper");

    assert!(store.committed_version().unwrap() > held);
    assert_eq!(
        store.retention_floor().unwrap(),
        held,
        "the floor may not pass a reader that is still reading"
    );
    assert_eq!(store.live_snapshots(), 1);
    assert!(store.oldest_snapshot_age().is_some());

    drop(reader);
    assert_eq!(
        store.retention_floor().unwrap(),
        store.committed_version().unwrap()
    );
}

#[test]
fn a_transaction_dropped_without_commit_or_rollback_releases_its_snapshot() {
    // This is the leak the design is shaped around. A transaction that is simply
    // let go of would otherwise pin the floor for the life of the process — not
    // with an error, but as space that never comes back.
    let store = store();
    let _ = tree(&store);
    let version = store.committed_version().unwrap();

    {
        let transaction = store.begin().unwrap();
        assert_eq!(store.live_snapshots(), 1);
        // No commit. No rollback. Just let go.
        let _ = transaction.snapshot();
    }

    assert_eq!(
        store.live_snapshots(),
        0,
        "the snapshot outlived its reader"
    );
    assert_eq!(store.retention_floor().unwrap(), version);
}

#[test]
fn a_committed_transaction_releases_its_snapshot_too() {
    let store = store();
    let (namespace, database, table) = tree(&store);
    let at = RecordAddress::new(
        tessari_types::NamespaceId::new(namespace),
        tessari_types::DatabaseId::new(database),
        tessari_types::TableId::new(table),
        RecordId::Int(1),
    );

    write(&store, &at, "ada");
    assert_eq!(store.live_snapshots(), 0);

    let transaction = store.begin().unwrap();
    transaction.rollback();
    assert_eq!(store.live_snapshots(), 0);
}

#[test]
fn two_readers_at_one_sequence_both_have_to_finish_before_the_floor_moves() {
    let store = store();
    let _ = tree(&store);
    let first = store.begin().unwrap();
    let second = store.begin().unwrap();
    let held = first.snapshot();
    assert_eq!(second.snapshot(), held, "both began at the same tail");
    assert_eq!(store.live_snapshots(), 1, "one sequence, two holders");

    drop(first);
    assert_eq!(
        store.retention_floor().unwrap(),
        held,
        "one reader is still working at it"
    );

    drop(second);
    assert_eq!(store.live_snapshots(), 0);
}

#[test]
fn a_cloned_store_shares_one_registry_because_it_is_one_store() {
    // A floor computed from half the live readers would reclaim versions the
    // other half is still reading.
    let store = store();
    let _ = tree(&store);
    let clone = store.clone();

    let reader = clone.begin().unwrap();
    let held = reader.snapshot();
    assert_eq!(store.live_snapshots(), 1);
    assert_eq!(store.retention_floor().unwrap(), held);

    drop(reader);
    assert_eq!(store.live_snapshots(), 0);
}

fn record(store: &Store, namespace: u32, database: u32, table: u32, id: i64) -> RecordAddress {
    let _ = store;
    RecordAddress::new(
        tessari_types::NamespaceId::new(namespace),
        tessari_types::DatabaseId::new(database),
        tessari_types::TableId::new(table),
        RecordId::Int(id),
    )
}

/// The `name` field of the record a reader resolves to.
fn name_seen_by(reader: &tessari_storage::Transaction<'_>, at: &RecordAddress) -> Option<String> {
    let payload = reader.get(at).unwrap()?;
    let Value::Object(object) = tessari_encoding::decode_payload(&payload).unwrap() else {
        panic!("expected an object");
    };
    match object.get("name") {
        Some(Value::String(text)) => Some(text.clone()),
        other => panic!("unexpected name: {other:?}"),
    }
}

#[test]
fn reclaiming_removes_older_versions_and_keeps_what_a_reader_at_the_floor_sees() {
    let store = store();
    let (ns, db, tb) = tree(&store);
    let at = record(&store, ns, db, tb, 1);

    write(&store, &at, "ada");
    write(&store, &at, "grace");
    write(&store, &at, "hopper");

    let removed = store
        .reclaim_table(at.namespace, at.database, at.table)
        .unwrap();
    assert_eq!(removed.versions, 2, "two versions nobody can reach");
    assert_eq!(removed.records, 0);

    // Asserted by reading rather than by counting keys, because over-reclaiming
    // does not raise anything — it just answers with an older value.
    let reader = store.begin().unwrap();
    assert_eq!(name_seen_by(&reader, &at).as_deref(), Some("hopper"));
}

#[test]
fn a_held_snapshot_keeps_every_version_it_can_still_reach() {
    // The registry's whole purpose: reclaiming while a reader is mid-work must
    // not change what that reader sees.
    let store = store();
    let (ns, db, tb) = tree(&store);
    let at = record(&store, ns, db, tb, 1);

    write(&store, &at, "ada");
    let reader = store.begin().unwrap();
    write(&store, &at, "grace");
    write(&store, &at, "hopper");

    let removed = store
        .reclaim_table(at.namespace, at.database, at.table)
        .unwrap();
    assert_eq!(
        removed.versions, 0,
        "the reader holds the floor, and nothing is older than what it sees"
    );
    assert_eq!(name_seen_by(&reader, &at).as_deref(), Some("ada"));

    // Once it finishes, the same pass can do its work.
    drop(reader);
    let removed = store
        .reclaim_table(at.namespace, at.database, at.table)
        .unwrap();
    assert_eq!(removed.versions, 2);

    let after = store.begin().unwrap();
    assert_eq!(name_seen_by(&after, &at).as_deref(), Some("hopper"));
}

#[test]
fn a_deleted_record_stops_costing_space() {
    let store = store();
    let (ns, db, tb) = tree(&store);
    let at = record(&store, ns, db, tb, 1);

    write(&store, &at, "ada");
    write(&store, &at, "grace");
    let mut transaction = store.begin().unwrap();
    transaction.delete(at.clone());
    transaction.commit().unwrap();

    let removed = store
        .reclaim_table(at.namespace, at.database, at.table)
        .unwrap();
    assert_eq!(removed.records, 1, "the tombstone itself went too");
    assert_eq!(removed.versions, 3, "two writes and the tombstone");

    // A reader that finds a tombstone and one that finds nothing reach the same
    // conclusion, which is what makes removing it safe.
    let reader = store.begin().unwrap();
    assert!(reader.get(&at).unwrap().is_none());
}

#[test]
fn reclaiming_twice_removes_nothing_the_second_time() {
    let store = store();
    let (ns, db, tb) = tree(&store);
    let at = record(&store, ns, db, tb, 1);
    write(&store, &at, "ada");
    write(&store, &at, "grace");

    let first = store
        .reclaim_table(at.namespace, at.database, at.table)
        .unwrap();
    assert!(first.versions > 0);
    let second = store
        .reclaim_table(at.namespace, at.database, at.table)
        .unwrap();
    assert_eq!(second.versions, 0);
}

#[test]
fn reclaiming_does_not_disturb_a_neighbouring_record() {
    let store = store();
    let (ns, db, tb) = tree(&store);
    let first = record(&store, ns, db, tb, 1);
    let second = record(&store, ns, db, tb, 2);

    write(&store, &first, "ada");
    write(&store, &first, "grace");
    write(&store, &second, "hopper");

    let removed = store
        .reclaim_table(first.namespace, first.database, first.table)
        .unwrap();
    assert_eq!(removed.versions, 1, "only the superseded one");

    let reader = store.begin().unwrap();
    assert_eq!(name_seen_by(&reader, &first).as_deref(), Some("grace"));
    assert_eq!(name_seen_by(&reader, &second).as_deref(), Some("hopper"));
}

// ── What a read outside the answerable window gets ─────────────────────────
//
// `Store::begin_at` refuses on both sides, and its doc gives the reason: each
// alternative is "a plausible wrong number that nothing reports". Until W171
// neither refusal had a test — the two variants appeared in the conformance
// error mapping and nowhere else, so the decision was held by the code alone and
// a refactor could have dropped it without failing anything.
//
// The bound `begin_at` checks is `reclaim_floor`, not `retention_floor`, and the
// difference is the whole design. `retention_floor` is the *live* bound (the
// oldest snapshot being read from, else the committed tail) and it answers "what
// may a pass remove right now"; it moves down when a reader arrives.
// `reclaim_floor` is durable and monotonic, written in the same batch as the
// removals, and answers "what has actually been removed" — which is the only one
// a historical read can be judged against. Collapsing them would either refuse
// answerable reads or, worse, admit unanswerable ones.

#[test]
fn a_read_below_the_reclaim_floor_is_refused_rather_than_answered_from_what_survived() {
    let store = store();
    let (ns, db, tb) = tree(&store);
    let at = record(&store, ns, db, tb, 1);

    let first = write(&store, &at, "ada");
    write(&store, &at, "grace");
    write(&store, &at, "hopper");

    let removed = store
        .reclaim_table(at.namespace, at.database, at.table)
        .unwrap();
    assert_eq!(removed.versions, 2);

    // `first` is now unanswerable: the version that would have answered it is
    // gone. The failure this refusal prevents is not an error — it is the read
    // quietly resolving to "hopper" and being believed as the past.
    match store.begin_at(first) {
        Err(tessari_storage::Error::VersionReclaimed { asked, floor }) => {
            assert_eq!(asked, first.get());
            assert!(
                floor > asked,
                "the floor has to be above what was asked, or the read was answerable"
            );
        }
        Err(other) => panic!("expected VersionReclaimed, got {other:?}"),
        Ok(_) => panic!("a read below the reclaim floor was allowed to begin"),
    }
}

#[test]
fn a_read_above_the_committed_tail_is_refused_rather_than_answered_with_the_present() {
    let store = store();
    let (ns, db, tb) = tree(&store);
    let at = record(&store, ns, db, tb, 1);
    write(&store, &at, "ada");

    let tail = store.committed_tail().unwrap();
    let ahead = Sequence::new(tail.get() + 1);

    // Answering with the present would make this succeed now and return a
    // different answer the next time it is asked, which is worse than refusing.
    match store.begin_at(ahead) {
        Err(tessari_storage::Error::VersionInTheFuture {
            asked,
            tail: at_tail,
        }) => {
            assert_eq!(asked, ahead.get());
            assert_eq!(at_tail, tail.get());
        }
        Err(other) => panic!("expected VersionInTheFuture, got {other:?}"),
        Ok(_) => panic!("a read above the committed tail was allowed to begin"),
    }
}

#[test]
fn a_read_at_the_reclaim_floor_itself_is_answerable_and_exact() {
    let store = store();
    let (ns, db, tb) = tree(&store);
    let at = record(&store, ns, db, tb, 1);

    write(&store, &at, "ada");
    write(&store, &at, "grace");
    write(&store, &at, "hopper");
    store
        .reclaim_table(at.namespace, at.database, at.table)
        .unwrap();

    // The boundary is inclusive, and it has to be: reclamation keeps the newest
    // version at or below the floor, so the floor is exactly the oldest sequence
    // that still resolves to the right value rather than the first that does not.
    let floor = store.reclaim_floor().unwrap();
    let reader = store.begin_at(floor).unwrap();
    assert_eq!(name_seen_by(&reader, &at).as_deref(), Some("hopper"));
}

#[test]
fn a_store_that_has_never_reclaimed_answers_a_read_at_any_past_sequence() {
    let store = store();
    let (ns, db, tb) = tree(&store);
    let first = record(&store, ns, db, tb, 1);
    let second = record(&store, ns, db, tb, 2);

    let earlier = write(&store, &first, "ada");
    write(&store, &second, "grace");

    // One version each, so a pass removes nothing and the floor stays where it
    // was. This is the case that separates the two floors: the live bound has
    // already moved to the tail, while the durable one is still `ZERO` because
    // nothing has been removed. `begin_at` has to consult the second, or every
    // historical read fails the moment a newer write lands.
    let removed = store
        .reclaim_table(first.namespace, first.database, first.table)
        .unwrap();
    assert_eq!(removed.versions, 0, "one version each, nothing superseded");
    assert_eq!(store.reclaim_floor().unwrap(), Sequence::ZERO);
    assert!(
        store.retention_floor().unwrap() > earlier,
        "the live bound has moved past the sequence being read, which is the point"
    );

    let reader = store.begin_at(earlier).unwrap();
    assert_eq!(name_seen_by(&reader, &first).as_deref(), Some("ada"));
    assert_eq!(
        name_seen_by(&reader, &second),
        None,
        "and it reads the past, not the present"
    );
}

#[test]
fn a_live_reader_bounds_how_far_the_floor_rises_and_that_bound_outlives_the_reader() {
    let store = store();
    let (ns, db, tb) = tree(&store);
    let at = record(&store, ns, db, tb, 1);

    let first = write(&store, &at, "ada");
    let second = write(&store, &at, "grace");

    // Held at `second`, so the live bound stops there while the tail runs ahead.
    let held = store.begin().unwrap();
    write(&store, &at, "hopper");

    let removed = store
        .reclaim_table(at.namespace, at.database, at.table)
        .unwrap();
    assert_eq!(
        removed.versions, 1,
        "only what the held reader cannot reach"
    );
    assert_eq!(name_seen_by(&held, &at).as_deref(), Some("grace"));

    // The reader leaves, and the live bound jumps to the tail. What the pass
    // actually removed does not change when it does, and that is the two-floor
    // distinction stated as a behaviour: `second` stays answerable afterwards
    // because nothing removed the version that answers it. A `begin_at` checking
    // the live bound would start refusing this read the instant the reader
    // finished, having removed nothing in between.
    drop(held);
    assert!(store.retention_floor().unwrap() > second);
    assert_eq!(store.reclaim_floor().unwrap(), second);

    let reader = store.begin_at(second).unwrap();
    assert_eq!(name_seen_by(&reader, &at).as_deref(), Some("grace"));
    assert!(matches!(
        store.begin_at(first),
        Err(tessari_storage::Error::VersionReclaimed { .. })
    ));
}
