//! A plain index answers a transaction the store has moved past (Q-S3-34).
//!
//! An index entry describes the committed tail, so a transaction whose snapshot
//! is older could not believe one: a record that matched at the snapshot and
//! changed since had lost its entry, and one that started matching since had
//! gained one. The store therefore withheld every index from such a
//! transaction, and with concurrent writers that is every multi-statement
//! transaction — `USING INDEX` refused it and an unasserted read scanned.
//!
//! The commit that removes a plain entry now keeps what it said, as a retired
//! entry stamped with the version that removed it. A transaction behind the
//! tail reads the live entries and the ones retired after its snapshot, and
//! re-tests every candidate at its own snapshot — a superset narrowed by the
//! exact check, which is the scan's answer at an index's cost.
//!
//! Each case pins both directions: the record that LEFT the value after the
//! snapshot must still be found (that is the half only the retired entry can
//! supply), and the record that ARRIVED after it must not be.

use std::sync::Arc;
use tessari_session::RunNow;

use tessari_encoding::KeyKind;
use tessari_kv::{KeyRange, KvBackend, MemoryBackend, ScanDirection, ScanRequest};
use tessari_session::{Error, Outcome, Parameters, Session};
use tessari_storage::Store;

fn store() -> Store {
    Store::open(Arc::new(MemoryBackend::new()) as Arc<dyn KvBackend>).unwrap()
}

/// How many retired entries the store holds, read off the backend directly so
/// the count does not depend on any reader of them.
fn retired(backend: &Arc<MemoryBackend>) -> usize {
    let tag = [KeyKind::RetiredIndex.tag()];
    backend
        .scan(&ScanRequest {
            keyspace: KeyKind::RetiredIndex.keyspace(),
            range: KeyRange::prefix(&tag),
            direction: ScanDirection::Forward,
            limit: None,
        })
        .unwrap()
        .len()
}

const SCHEMA: &str = "\
DEFINE NAMESPACE prod; USE NAMESPACE prod;
DEFINE DATABASE shop; USE DATABASE shop;
DEFINE COLLECTION t;
DEFINE COLLECTION other;
DEFINE INDEX by_k ON t FIELDS k;
DEFINE INDEX by_code ON t FIELDS code UNIQUE;
CREATE t:1 = { k: 'a', code: 'x1' };
CREATE t:2 = { k: 'b', code: 'x2' };
CREATE t:4 = { k: 'c', code: 'x4' };
";

fn session(store: &Store) -> Session<'_> {
    let mut session = Session::new(store);
    session
        .run_now("USE NAMESPACE prod; USE DATABASE shop;")
        .unwrap();
    session
}

fn ids(outcomes: &[Outcome]) -> Vec<String> {
    let Some(Outcome::Records { records, .. }) = outcomes.last() else {
        panic!("a read answered with {:?}", outcomes.last());
    };
    records.iter().map(|(id, _)| id.to_string()).collect()
}

/// Moves `t:1` from `a` to `b`, adds `t:3` under `a`, and moves `t:4` from `c`
/// to `d` — every direction an entry can take — from a second session, so they
/// commit after the held transaction began.
const MOVES: &str = "\
UPDATE t:1 MERGE { k: 'b' };
CREATE t:3 = { k: 'a', code: 'x3' };
UPDATE t:4 MERGE { k: 'd' };
CREATE other:1 = { note: 'an unrelated commit' };
";

#[test]
fn an_equality_answers_as_of_the_snapshot_through_the_index() {
    let store = store();
    Session::new(&store).run_now(SCHEMA).unwrap();
    let mut held = session(&store);
    let mut writer = session(&store);
    let read: tessari_session::Result<(Vec<String>, Vec<String>)> = held.atomically(|work| {
        writer.run_now(MOVES)?;
        let a = work.run_with(
            "SELECT * FROM t WHERE k = 'a' USING INDEX by_k;",
            &Parameters::new(),
        )?;
        let b = work.run_with(
            "SELECT * FROM t WHERE k = 'b' USING INDEX by_k;",
            &Parameters::new(),
        )?;
        Ok((ids(&a), ids(&b)))
    });
    let (a, b) = read.unwrap();
    // `t:1` left `a` after the snapshot and is still found; `t:3` arrived after
    // it and is not.
    assert_eq!(a, vec!["1".to_owned()]);
    // `t:1` holds `b` only at the tail, which this transaction does not see.
    assert_eq!(b, vec!["2".to_owned()]);
}

#[test]
fn a_range_answers_as_of_the_snapshot_through_the_index() {
    let store = store();
    Session::new(&store).run_now(SCHEMA).unwrap();
    let mut held = session(&store);
    let mut writer = session(&store);
    let read = held.atomically(|work| {
        writer.run_now(MOVES)?;
        work.run_with(
            "SELECT * FROM t WHERE k >= 'a' AND k <= 'c' USING INDEX by_k;",
            &Parameters::new(),
        )
        .map(|outcomes| ids(&outcomes))
    });
    // All three as they stood: `t:4` left the range after the snapshot.
    assert_eq!(
        read.unwrap(),
        vec!["1".to_owned(), "2".to_owned(), "4".to_owned()]
    );
}

#[test]
fn the_same_read_at_the_tail_sees_the_moves() {
    let store = store();
    Session::new(&store).run_now(SCHEMA).unwrap();
    session(&store).run_now(MOVES).unwrap();
    let mut reader = session(&store);
    let a = reader
        .run_now("SELECT * FROM t WHERE k = 'a' USING INDEX by_k;")
        .unwrap();
    // The retired entry of `t:1` must not reach a reader that sees its move.
    assert_eq!(ids(&a), vec!["3".to_owned()]);
}

#[test]
fn a_unique_index_is_still_withheld_behind_the_tail() {
    // A unique entry's key is the value it claims, so a removed claim cannot be
    // kept beside the live one; that index keeps the old guard.
    let store = store();
    Session::new(&store).run_now(SCHEMA).unwrap();
    let mut held = session(&store);
    let mut writer = session(&store);
    let read = held.atomically(|work| {
        writer.run_now(MOVES)?;
        work.run_with(
            "SELECT * FROM t WHERE code = 'x1' USING INDEX by_code;",
            &Parameters::new(),
        )
    });
    match read {
        Err(Error::IndexNotUsed { expected, took, .. }) => {
            assert_eq!(expected, "by_code");
            assert_eq!(took, "no index");
        }
        other => panic!("a unique index answered behind the tail: {other:?}"),
    }
}

#[test]
fn a_new_store_keeps_retired_entries_from_its_first_version() {
    let store = store();
    assert_eq!(
        store.retired_since().unwrap(),
        Some(tessari_types::Sequence::ZERO)
    );
}

#[test]
fn retired_entries_are_kept_while_a_reader_needs_them_and_reclaimed_after() {
    let backend = Arc::new(MemoryBackend::new());
    let store = Store::open(Arc::clone(&backend) as Arc<dyn KvBackend>).unwrap();
    Session::new(&store).run_now(SCHEMA).unwrap();
    assert_eq!(retired(&backend), 0, "nothing has been removed yet");

    let mut held = session(&store);
    let mut writer = session(&store);
    held.atomically(|_| -> tessari_session::Result<()> {
        writer.run_now(MOVES)?;
        // `t:1` lost `a` and `t:4` lost `c`: two retired entries, and a reader
        // older than both is still open, so neither may go.
        assert_eq!(retired(&backend), 2);
        assert_eq!(store.reclaim_retired_entries().unwrap(), 0);
        assert_eq!(retired(&backend), 2);
        Ok(())
    })
    .unwrap();

    // The reader is gone; no snapshot predates the removals any more.
    assert_eq!(store.reclaim_retired_entries().unwrap(), 2);
    assert_eq!(retired(&backend), 0);
}

#[test]
fn an_update_that_keeps_its_value_retires_nothing() {
    let backend = Arc::new(MemoryBackend::new());
    let store = Store::open(Arc::clone(&backend) as Arc<dyn KvBackend>).unwrap();
    Session::new(&store).run_now(SCHEMA).unwrap();
    session(&store)
        .run_now("UPDATE t:1 MERGE { note: 'k is unchanged' };")
        .unwrap();
    assert_eq!(retired(&backend), 0);
}

#[test]
fn a_historical_read_below_what_reclamation_removed_takes_the_scan_and_stays_exact() {
    // Reclaiming removes the retired entries a read below the floor would need,
    // so such a read must not be served from what is left of them.
    let store = store();
    let mut session = Session::new(&store);
    session.run_now(SCHEMA).unwrap();
    let before = store.committed_version().unwrap().get();
    session.run_now(MOVES).unwrap();
    assert!(store.reclaim_retired_entries().unwrap() > 0);

    let read = format!("SELECT * FROM t WHERE k = 'a' VERSION {before};");
    assert_eq!(ids(&session.run_now(&read).unwrap()), vec!["1".to_owned()]);
    match session.run_now(&format!(
        "SELECT * FROM t WHERE k = 'a' USING INDEX by_k VERSION {before};"
    )) {
        Err(Error::IndexNotUsed { took, .. }) => assert_eq!(took, "no index"),
        other => panic!("a read below the reclaimed floor was served by the index: {other:?}"),
    }
}

#[test]
fn a_historical_read_above_it_is_served_by_the_index_and_exact() {
    let store = store();
    let mut session = Session::new(&store);
    session.run_now(SCHEMA).unwrap();
    let before = store.committed_version().unwrap().get();
    session.run_now(MOVES).unwrap();
    let outcomes = session
        .run_now(&format!(
            "SELECT * FROM t WHERE k = 'a' USING INDEX by_k VERSION {before};"
        ))
        .unwrap();
    assert_eq!(ids(&outcomes), vec!["1".to_owned()]);
}
