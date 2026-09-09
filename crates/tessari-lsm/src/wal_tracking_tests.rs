//! What `track_and_verify_wals_in_manifest` buys, on a state that is built
//! rather than caught.
//!
//! # Why this is not in `tests/recovery.rs` with the rest of readiness row 6
//!
//! The behaviour is simple — a write-ahead file the engine has closed and
//! recorded cannot go missing without the store refusing to open — and getting
//! a store into that state from outside the crate is not. The integration test
//! that used to assert it *waited* for the state: it wrote until the live
//! `.log` name changed and grabbed the superseded name if it was still there.
//! That is a window, it was tuned twice, and it failed a third time under load
//! (Q-490).
//!
//! The window is narrow for a reason that is not scheduling. This store's
//! memtable trigger is `db_write_buffer_size` — a whole-database ceiling, which
//! is memory pressure — so crossing it switches the memtable *and* forces the
//! flush that frees the memory. The event that CLOSES a write-ahead file and
//! the event that makes it OBSOLETE are the same event. A fourth tuning would
//! fail for the same reason as the first three.
//!
//! So the state is forced instead. Four things are changed for the preparation
//! only, they are reachable only from inside the crate — which is what this
//! module is doing here — and each one closes a different route to a flush.
//! Three were predictable from the options; the fourth was not, and the first
//! two attempts at this construction failed for the same reason the test they
//! replace did:
//!
//! - the whole-database memtable ceiling is off, so a switch comes from a small
//!   per-region budget rather than from memory pressure;
//! - the write-ahead ceiling is raised, because its default is derived from the
//!   memtable budget the line above just made small — leave it and the engine
//!   flushes to reclaim write-ahead space instead;
//! - a flush waits for more immutable memtables than this preparation will ever
//!   produce, so a switch leaves the closed file needed rather than obsolete;
//! - **atomic flush is off**, and this is the one worth remembering: under
//!   `atomic_flush` the flush that a switch schedules does not consult
//!   `min_write_buffer_number_to_merge` at all, so the rule above is simply not
//!   applied and every switch flushes. With it left on, the preparation rotated
//!   a hundred times without ever holding a closed file — the same window as
//!   before, rebuilt by accident.
//!
//! None of the four reaches the store. The assertion runs against
//! [`crate::LsmBackend::open`] with the real options, and what is on disk when
//! it does is what those options describe: a manifest that records a closed
//! write-ahead file, and the file. Atomic flush decides *which regions flush
//! together*, not what a closed write-ahead file is.
//!
//! # And why the writer is killed rather than closed
//!
//! A clean close flushes, and a flush is exactly what makes the file obsolete.
//! The writer is a child process that prepares the state, announces it, and
//! blocks until the parent kills it — the same shape `tests/durability.rs`
//! already uses, and for the same reason: a clean close tests flush, not
//! recovery.

#![allow(clippy::panic, clippy::unwrap_used, clippy::indexing_slicing)]

use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use rocksdb::{Cache, ColumnFamilyDescriptor, DB, WriteOptions};
use tessari_kv::Keyspace;

use crate::backend::LsmBackend;
use crate::options::{Durability, StoreConfig, database_options, regions};

/// Names the store directory for the child.
const STORE_PATH: &str = "TESSARI_LSM_WAL_TRACKING_STORE";

/// The per-region memtable budget during the preparation. Small enough that a
/// couple of hundred records cross it several times.
const REGION_MEMTABLE_BYTES: usize = 64 * 1024;

/// How many memtables may accumulate, and how many a flush waits for. They are
/// the same number on purpose: nothing this test writes can reach it, so no
/// flush becomes pending and no closed write-ahead file is reclaimed.
const MEMTABLES: i32 = 8;

/// One live write-ahead file and two the engine has closed.
const WANTED_FILES: usize = 3;

/// A write-ahead ceiling far above anything this preparation writes, so that no
/// flush is ever forced to reclaim write-ahead space.
const WAL_CEILING_BYTES: u64 = 1 << 30;

/// Bytes per record during the preparation.
const RECORD_BYTES: usize = 4 * 1024;

/// A bound on the preparation, so a child that is never going to rotate ends
/// with an assertion rather than by filling a disk.
const CHILD_GIVES_UP_AFTER: usize = 4_000;

/// The write-ahead files of a store, oldest first.
fn write_ahead_files(path: &Path) -> Vec<PathBuf> {
    let Ok(entries) = fs::read_dir(path) else {
        return Vec::new();
    };
    let mut found: Vec<PathBuf> = entries
        .filter_map(std::result::Result::ok)
        .map(|entry| entry.path())
        .filter(|held| held.extension().is_some_and(|held| held == "log"))
        .collect();
    found.sort();
    found
}

/// Open the store with the four preparation changes described above.
///
/// The cache is returned with the database because the region options hold it;
/// dropping it here would leave the store's block cache owned by nobody.
fn opened_holding_its_closed_files(path: &Path) -> (DB, Cache, WriteOptions) {
    let config = StoreConfig::new(Durability::PowerLossSafe);
    let cache = Cache::new_lru_cache(config.block_cache_bytes);

    let mut database = database_options(&config, true);
    // Off, so that crossing a budget is not also a demand for memory back.
    database.set_db_write_buffer_size(0);
    // The other pressure that forces a flush: when the write-ahead files
    // together exceed this, the engine flushes whichever regions hold the
    // oldest of them so it can delete them. The default is derived from the
    // memtable budget, which the line above just made small, so it has to be
    // raised with it.
    database.set_max_total_wal_size(WAL_CEILING_BYTES);
    // And the one that is not visible in the option that appears to govern it.
    // Under atomic flush the flush a memtable switch schedules does not consult
    // `min_write_buffer_number_to_merge` below, so the floor is never applied
    // and every switch flushes. Left on, this preparation rotated a hundred
    // times and never held a closed file.
    database.set_atomic_flush(false);

    let descriptors = regions(&cache).into_iter().map(|(name, mut options)| {
        options.set_write_buffer_size(REGION_MEMTABLE_BYTES);
        options.set_max_write_buffer_number(MEMTABLES);
        options.set_min_write_buffer_number_to_merge(MEMTABLES);
        ColumnFamilyDescriptor::new(name, options)
    });

    let opened = DB::open_cf_descriptors(&database, path, descriptors).unwrap();
    // The store's own write options, and they are load-bearing here rather than
    // decorative: the engine records a closed write-ahead file in the MANIFEST
    // when that file is SYNCED, so a preparation that wrote without syncing
    // would leave a file the manifest never mentions — and removing it would
    // then be undetected for a reason that has nothing to do with the option
    // under test.
    (opened, cache, config.durability.write_options())
}

#[test]
#[ignore = "spawned by the tracked-write-ahead test; runs until it is killed"]
fn write_until_write_ahead_files_close_then_wait_to_be_killed() {
    let Ok(path) = std::env::var(STORE_PATH) else {
        panic!("{STORE_PATH} must name the store directory");
    };
    let path = PathBuf::from(path);
    let (database, _cache, durable) = opened_holding_its_closed_files(&path);
    let region = database.cf_handle(Keyspace::DATA.name()).unwrap();

    let mut written = 0_usize;
    while write_ahead_files(&path).len() < WANTED_FILES && written < CHILD_GIVES_UP_AFTER {
        let key = format!("record-{written:06}");
        database
            .put_cf_opt(region, key, [b'x'; RECORD_BYTES], &durable)
            .unwrap();
        written = written.saturating_add(1);
    }

    let held = write_ahead_files(&path);
    assert!(
        held.len() >= WANTED_FILES,
        "the store did not rotate its write-ahead file in {written} records; it holds {held:?}"
    );

    // The newest one the engine has closed. It cannot be reclaimed while this
    // process lives, so the parent is reading a fact and not catching a window.
    println!("prepared {}", held[held.len().saturating_sub(2)].display());
    std::io::stdout().flush().unwrap();

    // Blocks until the parent kills this process. Nothing runs on the way down,
    // which is the point: a clean close would flush, and a flush would take the
    // closed file with it.
    let mut ignored = String::new();
    let _ = std::io::stdin().read_line(&mut ignored);
}

#[test]
fn a_write_ahead_file_the_manifest_recorded_is_refused_when_it_is_gone() {
    // Readiness row 6, second half. `track_and_verify_wals_in_manifest` is set,
    // and this is what it buys. The engine's own words for it are `Corruption:
    // Missing WAL with log number: …`, which is the right shape — an unknown
    // number of records that may have been acknowledged is not something to
    // open around.
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("store");

    let mut child = Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "wal_tracking_tests::write_until_write_ahead_files_close_then_wait_to_be_killed",
            "--ignored",
            "--nocapture",
        ])
        .env(STORE_PATH, &path)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();

    let mut closed = None;
    {
        let stdout = child.stdout.take().unwrap();
        for line in BufReader::new(stdout).lines() {
            let line = line.unwrap();
            if let Some(named) = line.strip_prefix("prepared ") {
                closed = Some(PathBuf::from(named));
                break;
            }
        }
    }

    child.kill().unwrap();
    let _ = child.wait();

    let Some(closed) = closed else {
        panic!("the child did not report a closed write-ahead file");
    };
    // The construction's own claim, asserted rather than assumed: the file the
    // child named survived the kill, because nothing flushed on the way down.
    assert!(
        closed.exists(),
        "the closed write-ahead file {closed:?} was gone before the parent could remove it"
    );
    fs::remove_file(&closed).unwrap();

    let refused = LsmBackend::open(&path, StoreConfig::new(Durability::PowerLossSafe));
    assert!(
        refused.is_err(),
        "a store opened with a recorded write-ahead file missing"
    );
}
