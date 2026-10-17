//! Writes landed without a sync, and the sync that keeps them.

// bgv-allow(unwrap): test code, where a panic is the failure report.
#![allow(clippy::unwrap_used)]

use tessari_kv::{AsyncKvBackend as _, Key, Keyspace, Value, WriteBatch};
use tessarikv::Ticker;

use super::TessariKvBackend;
use crate::{Durability, StoreConfig};

fn put(n: u8) -> WriteBatch {
    WriteBatch::new().put(Keyspace::INDEX, Key::from_slice(&[n]), Value::new(vec![n]))
}

fn syncs(store: &TessariKvBackend) -> u64 {
    store
        .db()
        .ok()
        .and_then(tessarikv::Db::statistics)
        .map_or(0, |counted| counted.ticker(Ticker::WalSyncs))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_unsynced_apply_waits_for_the_device_only_when_synced_after() {
    let root = tempfile::tempdir().unwrap();
    let store = TessariKvBackend::open(root.path(), StoreConfig::new(Durability::PowerLossSafe))
        .await
        .unwrap();
    let before = syncs(&store);
    store.apply_unsynced(put(1)).await.unwrap();
    assert_eq!(syncs(&store), before, "an unsynced apply synced");
    store.apply(put(2)).await.unwrap();
    let after_commit = syncs(&store);
    assert!(
        after_commit > before,
        "a power-loss-safe commit did not sync"
    );
    store.apply_unsynced(put(3)).await.unwrap();
    store.sync_applied().await.unwrap();
    assert!(
        syncs(&store) > after_commit,
        "the sync of landed writes did not sync"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn writes_landed_unsynced_are_kept_once_synced_at_either_durability() {
    for durability in [Durability::PowerLossSafe, Durability::ProcessCrashSafe] {
        let root = tempfile::tempdir().unwrap();
        let store = TessariKvBackend::open(root.path(), StoreConfig::new(durability))
            .await
            .unwrap();
        for n in 0..3_u8 {
            store.apply_unsynced(put(n)).await.unwrap();
        }
        store.sync_applied().await.unwrap();
        store.close().await.unwrap();

        let reopened = TessariKvBackend::open(root.path(), StoreConfig::new(durability))
            .await
            .unwrap();
        for n in 0..3_u8 {
            assert_eq!(
                reopened
                    .get(Keyspace::INDEX, &Key::from_slice(&[n]))
                    .await
                    .unwrap(),
                Some(Value::new(vec![n])),
                "{durability:?}"
            );
        }
        reopened.close().await.unwrap();
    }
}
