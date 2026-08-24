//! What the engine does to a store that was interrupted, and to one that was
//! damaged.
//!
//! These are readiness rows 6 and 7, and both had stood at `no-evidence` since
//! the checklist was first written — not because they are hard but because the
//! store had never once executed the behaviour it depends on. A write-ahead log
//! whose tail is torn is what a power cut leaves behind, and a compaction is
//! what every level of the engine spends its life doing; a store that has never
//! recovered from the first, or asserted anything about the second, is trusting
//! two configuration lines.
//!
//! # What a torn tail is, and why it is tolerated rather than refused
//!
//! A write-ahead record is appended and then made durable. A machine that loses
//! power between those two things leaves a record that begins and does not end.
//! That is the **normal** shape of an interrupted write, and refusing to open
//! would turn every unclean shutdown into an outage — so the engine is set to
//! `TolerateCorruptedTailRecords`, which drops the incomplete record at the end
//! and keeps everything before it.
//!
//! The reason that is safe and not merely convenient: a record whose write did
//! not complete was never acknowledged to anybody, so dropping it loses a commit
//! nobody was told had happened.
//!
//! # And why a missing file is not the same thing
//!
//! A torn tail is one incomplete record at the end of a file the engine knows
//! about. A file that is *gone* is an unknown number of complete records, any of
//! which may have been acknowledged. Tolerating that would mean opening a store
//! and silently answering as though writes somebody was told had landed had
//! never happened — which is the difference between losing what was never
//! promised and losing what was.

#![allow(clippy::panic, clippy::unwrap_used, clippy::indexing_slicing)]

use std::fs;
use std::path::{Path, PathBuf};

use tessari_kv::{
    Key, KeyRange, Keyspace, KvBackend, ScanDirection, ScanRequest, Value, WriteBatch,
};
use tessari_lsm::{Durability, LsmBackend, StoreConfig};

fn config() -> StoreConfig {
    StoreConfig::new(Durability::PowerLossSafe)
}

fn key(n: usize) -> Key {
    Key::from_slice(format!("record-{n:06}").as_bytes())
}

fn payload(n: usize) -> Value {
    Value::from_slice(format!("payload for {n}").as_bytes())
}

/// Write `count` records, one batch each, and leave without closing.
///
/// Without the close, the write-ahead log is what holds them — which is the
/// state a machine is in when it loses power, and the only state in which these
/// tests mean anything.
fn written_and_abandoned(path: &Path, count: usize) {
    let store = LsmBackend::open(path, config()).unwrap();
    for n in 0..count {
        store
            .apply(WriteBatch::new().put(Keyspace::DATA, key(n), payload(n)))
            .unwrap();
    }
    drop(store);
}

/// The write-ahead files of a store, newest last.
fn logs(path: &Path) -> Vec<PathBuf> {
    let mut found: Vec<PathBuf> = fs::read_dir(path)
        .unwrap()
        .filter_map(std::result::Result::ok)
        .map(|entry| entry.path())
        .filter(|held| held.extension().is_some_and(|held| held == "log"))
        .collect();
    found.sort();
    found
}

/// How many records of `count` a reopened store still holds.
fn present(path: &Path, count: usize) -> usize {
    let store = LsmBackend::open(path, config()).unwrap();
    let held = (0..count)
        .filter(|n| store.get(Keyspace::DATA, &key(*n)).unwrap().is_some())
        .count();
    drop(store);
    held
}

#[test]
fn a_torn_trailing_record_is_tolerated_and_everything_before_it_survives() {
    // Readiness row 6, first half. The store is left without a clean close, so
    // its records are in the write-ahead log, and then the last bytes of that
    // log are removed — which is what a machine losing power mid-append leaves.
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("store");
    const RECORDS: usize = 200;

    written_and_abandoned(&path, RECORDS);
    let held = logs(&path);
    assert!(!held.is_empty(), "no write-ahead file to tear");
    let newest = held.last().unwrap();
    let length = fs::metadata(newest).unwrap().len();
    assert!(length > 64, "the write-ahead file is too small to tear");

    // Take the tail off, the way an interrupted append leaves it.
    let file = fs::OpenOptions::new().write(true).open(newest).unwrap();
    file.set_len(length.saturating_sub(48)).unwrap();
    drop(file);

    // It opens. That is the row.
    let survived = present(&path, RECORDS);
    assert!(
        survived > 0,
        "the store opened but held nothing; the tail took everything with it"
    );
    assert!(
        survived <= RECORDS,
        "more records survived than were written"
    );
    // Everything before the torn record is there: the surviving set is a
    // prefix, not a scatter. A recovery that kept record 100 and dropped record
    // 50 would mean the log is not being replayed in order.
    for n in 0..survived {
        let store = LsmBackend::open(&path, config()).unwrap();
        assert!(
            store.get(Keyspace::DATA, &key(n)).unwrap().is_some(),
            "record {n} is missing while {survived} survived — recovery is not a prefix"
        );
        drop(store);
    }
}

#[test]
fn a_write_ahead_file_the_manifest_recorded_is_refused_when_it_is_gone() {
    // Readiness row 6, second half. `track_and_verify_wals_in_manifest` is set,
    // and this is what it buys: a write-ahead file the engine has closed and
    // recorded cannot go missing without the store refusing to open.
    //
    // The engine's own words for it are `Corruption: Missing WAL with log
    // number: …`, which is the right shape — an unknown number of records that
    // may have been acknowledged is not something to open around.
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("store");
    let mut small = config();
    // Small enough that the engine rotates its write-ahead file, so there is a
    // closed one to remove rather than only the live one.
    small.memtable_bytes = 512 * 1024;

    // **Written until rotation is observed**, rather than a byte count chosen to
    // produce it. A fixed count made this fail its own *precondition* under load
    // — the engine's arena allocates in blocks larger than this budget, so how
    // much has to be written before a file closes is not a number a test can
    // know. It failed twice, days apart, and never on a re-run: exactly what a
    // precondition that is a guess looks like.
    //
    // Rotation is detected by the **live file's name changing**, not by two
    // files being seen at once. The engine reclaims a closed write-ahead file as
    // soon as its memtable flushes, so "two exist" is a window that a directory
    // read can pass straight over — and under load it does. A name that has been
    // superseded is evidence the rotation happened whether or not the older file
    // has already gone, and the file is grabbed at that moment rather than
    // looked for again after the store closes, because closing flushes and a
    // flush is what removes it.
    let mut closed = None;
    {
        let store = LsmBackend::open(&path, small).unwrap();
        let mut live = logs(&path).last().cloned();
        let mut n = 0_usize;
        while closed.is_none() && n < 40_000 {
            store
                .apply(WriteBatch::new().put(
                    Keyspace::DATA,
                    key(n),
                    Value::from_slice(&[b'x'; 1024]),
                ))
                .unwrap();
            n = n.saturating_add(1);
            let held = logs(&path);
            let newest = held.last().cloned();
            if newest != live {
                // The previous live file has been superseded. It is the closed,
                // recorded one this test needs — if the engine has not already
                // reclaimed it.
                closed = live.filter(|held| held.exists());
                live = newest;
            }
        }
        drop(store);
    }

    // A precondition that could not be established says so, and says what it
    // saw. Reporting it as "the engine did not rotate" would blame the engine
    // for a race in the test, which is how the same failure got tuned twice.
    let Some(closed) = closed.filter(|held| held.exists()) else {
        panic!(
            "no closed write-ahead file could be caught before the engine reclaimed it; \
             the store now holds {:?}",
            logs(&path)
        );
    };
    fs::remove_file(&closed).unwrap();

    let refused = LsmBackend::open(&path, small);
    assert!(
        refused.is_err(),
        "a store opened with a recorded write-ahead file missing"
    );
}

#[test]
fn losing_the_live_write_ahead_file_loses_its_writes_and_opens_a_consistent_older_store() {
    // The other half of the same boundary, pinned here because it is surprising
    // and because finding it again the hard way would cost somebody a day.
    //
    // The **live** write-ahead file is not in the manifest — the engine records
    // one when it closes it — so removing it is not detected. Two hundred
    // acknowledged, power-loss-safe commits are gone and the store opens
    // holding none of them, with no error anywhere.
    //
    // That is not a defect to fix, and saying why matters. The live file *is*
    // the store's newest data; once it is gone there is nothing left to notice
    // with. And because `atomic_flush` keeps every region consistent with it,
    // what opens is a consistent **older** store rather than a damaged one —
    // the records and the position that accounts for them went together.
    //
    // What it means operationally is the thing to carry: anything that removes
    // `*.log` from a store directory — a backup tool copying "just the data", a
    // cleanup script, an operator reclaiming space — silently discards the most
    // recent writes. Q-51.
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("store");
    const RECORDS: usize = 200;

    written_and_abandoned(&path, RECORDS);
    let held = logs(&path);
    assert_eq!(held.len(), 1, "this test needs exactly the live file");
    fs::remove_file(&held[0]).unwrap();

    let store = LsmBackend::open(&path, config()).unwrap();
    let survived = (0..RECORDS)
        .filter(|n| store.get(Keyspace::DATA, &key(*n)).unwrap().is_some())
        .count();
    assert_eq!(
        survived, 0,
        "some records survived without the file that held them"
    );
    // Consistent rather than damaged: it opens, it answers, and it records no
    // background failure. It is simply an earlier store.
    assert_eq!(store.background_errors().unwrap(), 0);
    drop(store);
}

#[test]
fn a_forced_compaction_changes_how_records_are_stored_and_not_what_they_are() {
    // Readiness row 7. The engine compacts on its own schedule, so before this
    // there was no test in which a compaction had ever run — the behaviour every
    // level of the store depends on had never once been executed deliberately.
    //
    // What is asserted is the property that matters: a compaction rewrites
    // storage and never answers. One that dropped a live record, or resurrected
    // a deleted one, would do it silently and no read would raise anything.
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("store");
    const RECORDS: usize = 2_000;

    let store = LsmBackend::open(&path, config()).unwrap();
    for n in 0..RECORDS {
        store
            .apply(WriteBatch::new().put(Keyspace::DATA, key(n), payload(n)))
            .unwrap();
    }
    // Delete every third, so the compaction has tombstones to apply rather than
    // only levels to merge.
    for n in (0..RECORDS).step_by(3) {
        store
            .apply(WriteBatch::new().delete(Keyspace::DATA, key(n)))
            .unwrap();
    }

    let before = scan(&store);
    store.compact().unwrap();
    let after = scan(&store);

    assert_eq!(before, after, "a compaction changed what the store answers");
    // And it was not a no-op on an empty store: the records that should be gone
    // are gone and the rest are all there.
    let live = RECORDS - RECORDS.div_ceil(3);
    assert_eq!(after.len(), live, "the wrong number of records survived");
    for n in 0..RECORDS {
        let held = store.get(Keyspace::DATA, &key(n)).unwrap();
        if n % 3 == 0 {
            assert!(held.is_none(), "record {n} was deleted and came back");
        } else {
            assert_eq!(
                held.map(|found| found.as_slice().to_vec()),
                Some(payload(n).as_slice().to_vec()),
                "record {n} did not survive the compaction"
            );
        }
    }
    drop(store);
}

#[test]
fn a_compaction_survives_a_reopen_with_the_same_answers() {
    // A compaction rewrites files on disk, so the assertion above is only half
    // of it: what the store answers from its own memory afterwards, and what it
    // answers after being closed and opened again, have to agree.
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("store");
    const RECORDS: usize = 500;

    let store = LsmBackend::open(&path, config()).unwrap();
    for n in 0..RECORDS {
        store
            .apply(WriteBatch::new().put(Keyspace::DATA, key(n), payload(n)))
            .unwrap();
    }
    for n in (0..RECORDS).step_by(5) {
        store
            .apply(WriteBatch::new().delete(Keyspace::DATA, key(n)))
            .unwrap();
    }
    store.compact().unwrap();
    let compacted = scan(&store);
    store.close().unwrap();

    let reopened = LsmBackend::open(&path, config()).unwrap();
    assert_eq!(compacted, scan(&reopened));
    assert_eq!(
        reopened.background_errors().unwrap(),
        0,
        "the compaction recorded a background failure"
    );
    drop(reopened);
}

/// Every key and value of the data region.
fn scan(store: &LsmBackend) -> Vec<(Vec<u8>, Vec<u8>)> {
    let request = ScanRequest {
        keyspace: Keyspace::DATA,
        range: KeyRange::all(),
        direction: ScanDirection::Forward,
        limit: None,
    };
    store
        .scan(&request)
        .unwrap()
        .into_iter()
        .map(|(key, value)| (key.as_slice().to_vec(), value.as_slice().to_vec()))
        .collect()
}
