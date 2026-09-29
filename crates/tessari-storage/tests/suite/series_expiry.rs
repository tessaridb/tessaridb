//! The removal pass takes a series' aged records as one range (G044 C11).
//!
//! What `series_floor` asserts about one stale record, asserted here at the
//! scale and in the company where the range form could go wrong: more records
//! than any batch, the log that must not learn of it, and a reader begun before
//! the floor moved.

use tessari_encoding::{RecordKey, StoreKey};
use tessari_kv::{KeyRange, Keyspace, ScanDirection, ScanRequest};
use tessari_storage::{Expired, RecordAddress, SeriesDeclaration, TableKind};
use tessari_types::IdentityKind;

use super::series_floor::{Fixture, OLD, RETAIN, identity, now_millis};

fn series() -> Fixture {
    Fixture::holding(
        TableKind::Series(SeriesDeclaration {
            retain: RETAIN,
            time: None,
            rollups: Vec::new(),
            rollup_of: None,
        }),
        IdentityKind::Uuid,
    )
}

fn expire(fixture: &Fixture) -> Expired {
    fixture
        .store
        .expire_series(fixture.namespace, fixture.database, fixture.table)
        .unwrap()
}

fn entries(fixture: &Fixture, keyspace: Keyspace) -> usize {
    fixture
        .backend
        .scan(&ScanRequest::new(keyspace, KeyRange::all()))
        .unwrap()
        .len()
}

/// Two thousand aged records, four times a batch of the pass it replaced — which
/// stopped after its first batch, because every later scan found that batch's
/// tombstones first and took them for the end.
#[test]
fn every_record_below_the_floor_goes_however_many_there_are() {
    let fixture = series();
    for n in 0..2_000_u64 {
        fixture.write(identity(fixture.base, OLD + n, 0xc0), "stale");
    }
    let before = fixture.labels();
    assert_eq!(fixture.stored_entries(), 2_002);

    assert_eq!(expire(&fixture).ranges, 1);
    assert_eq!(fixture.labels(), before);
    assert_eq!(
        fixture.stored_entries(),
        1,
        "only the current record is left"
    );
    assert_eq!(expire(&fixture), Expired::default());
}

/// The answer changed when the floor passed, so the removal is this node's
/// storage work and not a change a follower or a subscriber has to learn.
#[test]
fn the_pass_writes_nothing_to_the_log() {
    let fixture = series();
    let logged = entries(&fixture, Keyspace::LOG);
    assert_eq!(expire(&fixture).ranges, 1);
    assert_eq!(entries(&fixture, Keyspace::LOG), logged);
}

/// A transaction reads at the floor of the instant it began. A record that was
/// above that floor and has since fallen below the clock's is still in its
/// answer, so the pass must leave it until the transaction is gone.
#[test]
fn a_reader_begun_earlier_keeps_what_it_could_see() {
    let fixture = series();
    let base = now_millis();
    // Fifty milliseconds above the floor as it stands now.
    let hour = 60 * 60 * 1_000;
    let edge = identity(base, hour - 50, 0xd4);
    fixture.write(edge.clone(), "edge");
    let address = RecordAddress::new(
        fixture.namespace,
        fixture.database,
        fixture.table,
        edge.clone(),
    );

    let reader = fixture.store.begin().unwrap();
    assert!(reader.get(&address).unwrap().is_some(), "above its floor");
    // Until the clock has moved the floor past the record. A spin rather than
    // a sleep: it only has to observe the clock move.
    while now_millis() < base + 200 {
        std::hint::spin_loop();
    }
    assert!(
        fixture
            .store
            .begin()
            .unwrap()
            .get(&address)
            .unwrap()
            .is_none(),
        "below the floor a transaction begun now reads at"
    );

    // The fixture's two-hour-old record goes; the edge stays for the reader.
    assert_eq!(expire(&fixture).ranges, 1);
    let key = RecordKey::versions_prefix(fixture.namespace, fixture.database, fixture.table, &edge);
    let held = fixture
        .backend
        .scan(&ScanRequest {
            keyspace: RecordKey::keyspace(),
            range: KeyRange::prefix(&key),
            direction: ScanDirection::Forward,
            limit: None,
        })
        .unwrap();
    assert_eq!(held.len(), 1, "the edge's bytes are still there");
    assert!(
        reader.get(&address).unwrap().is_some(),
        "and still answered"
    );

    // Once the reader is gone, nothing holds the floor back.
    drop(reader);
    assert_eq!(expire(&fixture).ranges, 1);
    assert_eq!(
        fixture.stored_entries(),
        1,
        "only the current record is left"
    );
}

/// The node's cadence calls the store-wide sweep, which finds every series
/// table through the catalog.
#[test]
fn the_sweep_finds_every_series_table() {
    let fixture = series();
    assert_eq!(fixture.store.expire_every_series().unwrap().ranges, 1);
    assert_eq!(
        fixture.stored_entries(),
        1,
        "only the current record is left"
    );
    assert_eq!(
        fixture.store.expire_every_series().unwrap(),
        Expired::default()
    );
}
