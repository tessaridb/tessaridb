//! That a sweep does not fill the block cache, counted rather than timed.
//!
//! The behaviour under test is invisible from outside the engine: a sweep and a
//! scan answer the same records, and the only difference is what the cache holds
//! afterwards. Timing it would be a wall-clock assertion on a shared machine,
//! which `bgv-rocksdb` ref 12 says to avoid in favour of counters — they are
//! stable across noisy hosts and they name the mechanism rather than its shadow.
//!
//! The engine already keeps the counter: `enable_statistics()` is set in
//! `database_options`, and the whole set is readable as a DB property.

#![allow(clippy::panic, clippy::unwrap_used, clippy::indexing_slicing)]

use tessari_kv::{Key, Keyspace, KvBackend, ScanRequest, Value, WriteBatch};

use crate::backend::LsmBackend;
use crate::options::{Durability, StoreConfig};

/// A block cache small enough that a table of a few megabytes cannot fit in it.
const SMALL_CACHE_BYTES: usize = 512 * 1024;

/// Records written, and the size of each, so the table is comfortably larger
/// than the cache above.
const RECORDS: usize = 2_000;
const RECORD_BYTES: usize = 4 * 1024;

fn counter(store: &LsmBackend, name: &str) -> u64 {
    let stats = store
        .statistics()
        .expect("statistics are enabled in database_options");
    for line in stats.lines() {
        let mut parts = line.split_whitespace();
        if parts.next() != Some(name) {
            continue;
        }
        // `<name> COUNT : <n>` — the shape every ticker line has, so the count
        // is whatever follows the colon.
        let mut after = parts.skip_while(|word| *word != ":");
        after.next();
        if let Some(count) = after.next() {
            return count.parse().unwrap_or(0);
        }
    }
    panic!("no counter named {name} in the statistics");
}

fn filled(store: &LsmBackend) -> u64 {
    counter(store, "rocksdb.block.cache.data.add")
}

#[test]
fn a_sweep_reads_the_same_range_without_filling_the_block_cache() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("store");
    let mut config = StoreConfig::new(Durability::ProcessCrashSafe);
    config.block_cache_bytes = SMALL_CACHE_BYTES;

    let store = LsmBackend::open(&path, config).unwrap();
    for n in 0..RECORDS {
        store
            .apply(WriteBatch::new().put(
                Keyspace::DATA,
                Key::from_slice(format!("row-{n:06}").as_bytes()),
                Value::from_slice(&vec![b'x'; RECORD_BYTES]),
            ))
            .unwrap();
    }
    // Force the memtable out to files: a read served from the memtable touches
    // no block at all, so the counter would be flat for the wrong reason.
    store.compact().unwrap();

    let request = ScanRequest::new(Keyspace::DATA, tessari_kv::KeyRange::prefix(b"row-"));

    let before_sweep = filled(&store);
    let swept = store.sweep(&request).unwrap();
    let after_sweep = filled(&store);

    let before_scan = after_sweep;
    let scanned = store.scan(&request).unwrap();
    let after_scan = filled(&store);

    assert_eq!(
        swept.len(),
        RECORDS,
        "the sweep did not read the table it was given"
    );
    assert_eq!(
        swept, scanned,
        "the sweep and the scan answered differently"
    );

    let by_sweep = after_sweep.saturating_sub(before_sweep);
    let by_scan = after_scan.saturating_sub(before_scan);
    assert_eq!(
        by_sweep, 0,
        "the sweep added {by_sweep} blocks to the cache; the scan added {by_scan}"
    );
    assert!(
        by_scan > 0,
        "the scan added no blocks either, so this test is measuring nothing \
         (swept {by_sweep}, scanned {by_scan})"
    );
}
