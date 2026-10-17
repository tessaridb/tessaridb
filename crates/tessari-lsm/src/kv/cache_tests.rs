//! That a sweep does not fill the block cache, counted rather than timed.
//!
//! A sweep and a scan answer the same records, and the only difference is what the cache holds
//! afterwards, which no answer shows. The engine's own counters do: a range read twice finds its
//! blocks in the cache the second time only when the first read put them there.

// bgv-allow(unwrap): test code, where a panic is the failure report.
#![allow(clippy::unwrap_used)]

use tessari_kv::{AsyncKvBackend as _, Key, KeyRange, Keyspace, ScanRequest, Value, WriteBatch};
use tessarikv::Ticker;

use super::TessariKvBackend;
use crate::{Durability, StoreConfig};

/// Records written, and the size of each: a table of several megabytes, well inside the default
/// block cache, so a scan's blocks are all still cached when it runs again.
const RECORDS: usize = 2_000;
const RECORD_BYTES: usize = 4 * 1024;

fn hits(store: &TessariKvBackend) -> u64 {
    store
        .db()
        .ok()
        .and_then(tessarikv::Db::statistics)
        .map_or(0, |counted| counted.ticker(Ticker::BlockCacheHit))
}

/// The cache hits of reading `request` once more, after it was read once.
async fn second_read_hits(store: &TessariKvBackend, request: &ScanRequest, sweep: bool) -> u64 {
    let read = |request| async move {
        if sweep {
            store.sweep(request).await.unwrap()
        } else {
            store.scan(request).await.unwrap()
        }
    };
    let first = read(request).await;
    assert_eq!(first.len(), RECORDS, "the read did not cover the table");
    let before = hits(store);
    let second = read(request).await;
    assert_eq!(first, second, "one range answered two ways");
    hits(store).saturating_sub(before)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_sweep_reads_the_same_range_without_filling_the_block_cache() {
    let root = tempfile::tempdir().unwrap();
    let store = TessariKvBackend::open(
        root.path().join("store"),
        StoreConfig::new(Durability::ProcessCrashSafe),
    )
    .await
    .unwrap();
    for n in 0..RECORDS {
        store
            .apply(WriteBatch::new().put(
                Keyspace::DATA,
                Key::from_slice(format!("row-{n:06}").as_bytes()),
                Value::from_slice(&vec![b'x'; RECORD_BYTES]),
            ))
            .await
            .unwrap();
    }
    // Into table files: a read the in-memory table serves touches no block, so both counts
    // would be flat for the wrong reason.
    store.compact().await.unwrap();
    let request = ScanRequest::new(Keyspace::DATA, KeyRange::prefix(b"row-"));

    let by_sweep = second_read_hits(&store, &request, true).await;
    let by_scan = second_read_hits(&store, &request, false).await;
    assert_eq!(
        by_sweep, 0,
        "a second sweep found {by_sweep} blocks the first one cached"
    );
    // The control: a scan does fill the cache, so the zero above is about the sweep.
    assert!(
        by_scan > 0,
        "a second scan found nothing cached either, so this test measures nothing"
    );
}
