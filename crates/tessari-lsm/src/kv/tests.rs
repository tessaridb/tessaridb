//! The TessariKV backend against the shared contract, against the in-memory backend, and under
//! racing preconditions.

#![allow(clippy::unwrap_used, clippy::panic)]

use std::path::Path;
use std::sync::Arc;

use tessari_kv::{
    AsyncKvBackend, Key, KeyRange, Keyspace, KvBackend, MemoryBackend, Result, ScanRequest, Value,
    WriteBatch, conformance,
};
use tokio::runtime::Runtime;

use super::TessariKvBackend;
use crate::{Durability, StoreConfig};

/// The backend driven to completion call by call, so the synchronous suite can run against it.
/// Test code only: the library never blocks on its own futures.
struct Driven {
    backend: TessariKvBackend,
    runtime: Arc<Runtime>,
}

impl std::fmt::Debug for Driven {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.backend.fmt(f)
    }
}

impl KvBackend for Driven {
    fn name(&self) -> &'static str {
        AsyncKvBackend::name(&self.backend)
    }

    fn get(&self, keyspace: Keyspace, key: &Key) -> Result<Option<Value>> {
        self.runtime
            .block_on(AsyncKvBackend::get(&self.backend, keyspace, key))
    }

    fn scan(&self, request: &ScanRequest) -> Result<Vec<(Key, Value)>> {
        self.runtime
            .block_on(AsyncKvBackend::scan(&self.backend, request))
    }

    fn sweep(&self, request: &ScanRequest) -> Result<Vec<(Key, Value)>> {
        self.runtime
            .block_on(AsyncKvBackend::sweep(&self.backend, request))
    }

    fn first_of_each(
        &self,
        keyspace: Keyspace,
        ranges: &[KeyRange],
    ) -> Result<Vec<Option<(Key, Value)>>> {
        self.runtime.block_on(AsyncKvBackend::first_of_each(
            &self.backend,
            keyspace,
            ranges,
        ))
    }

    fn count(&self, keyspace: Keyspace, range: &KeyRange) -> Result<u64> {
        self.runtime
            .block_on(AsyncKvBackend::count(&self.backend, keyspace, range))
    }

    fn apply(&self, batch: WriteBatch) -> Result<()> {
        self.runtime
            .block_on(AsyncKvBackend::apply(&self.backend, batch))
    }

    fn contains(&self, keyspace: Keyspace, key: &Key) -> Result<bool> {
        self.runtime
            .block_on(AsyncKvBackend::contains(&self.backend, keyspace, key))
    }

    fn delete_range(&self, keyspace: Keyspace, range: &KeyRange) -> Result<()> {
        self.runtime
            .block_on(AsyncKvBackend::delete_range(&self.backend, keyspace, range))
    }
}

fn runtime() -> Arc<Runtime> {
    Arc::new(
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(4)
            .enable_all()
            .build()
            .unwrap(),
    )
}

async fn open(path: &Path) -> TessariKvBackend {
    TessariKvBackend::open(path, StoreConfig::new(Durability::ProcessCrashSafe))
        .await
        .unwrap()
}

fn key(bytes: &[u8]) -> Key {
    Key::from_slice(bytes)
}

fn value(bytes: &[u8]) -> Value {
    Value::from_slice(bytes)
}

#[test]
fn the_tessarikv_backend_satisfies_the_whole_contract() {
    let root = tempfile::tempdir().unwrap();
    let runtime = runtime();
    let mut index = 0;
    let results = conformance::run_all(|| {
        index += 1;
        let path = root.path().join(format!("store-{index}"));
        Driven {
            backend: runtime.block_on(open(&path)),
            runtime: Arc::clone(&runtime),
        }
    });
    assert_eq!(results.len(), conformance::check_count());
    let failures: Vec<String> = results
        .iter()
        .filter_map(|result| {
            result
                .failure
                .as_ref()
                .map(|reason| format!("{}: {reason}", result.name))
        })
        .collect();
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

/// A small deterministic generator, so a failing stream can be replayed from its seed.
struct Stream(u64);

impl Stream {
    fn next(&mut self) -> u64 {
        // xorshift64*
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_f491_4f6c_dd1d)
    }

    fn below(&mut self, bound: u64) -> u64 {
        self.next().checked_rem(bound).unwrap_or(0)
    }

    fn key(&mut self) -> Key {
        // A narrow alphabet, so writes collide with earlier keys and ranges hold several.
        let length = self.below(3).saturating_add(1);
        let bytes: Vec<u8> = (0..length)
            .map(|_| u8::try_from(self.below(6)).unwrap())
            .collect();
        Key::new(bytes)
    }

    fn keyspace(&mut self) -> Keyspace {
        let at = usize::try_from(self.below(2)).unwrap();
        [Keyspace::DATA, Keyspace::INDEX][at]
    }
}

/// Every pair of every keyspace, read forward and reversed, with each range's count and first
/// pair: what two backends must agree on after the same writes.
async fn picture(backend: &dyn AsyncKvBackend, ranges: &[KeyRange]) -> Vec<String> {
    let mut seen = Vec::new();
    for keyspace in [Keyspace::DATA, Keyspace::INDEX] {
        for range in ranges {
            let forward = backend
                .scan(&ScanRequest::new(keyspace, range.clone()))
                .await
                .unwrap();
            let reverse = backend
                .scan(&ScanRequest::new(keyspace, range.clone()).reversed())
                .await
                .unwrap();
            let limited = backend
                .scan(&ScanRequest::new(keyspace, range.clone()).with_limit(2))
                .await
                .unwrap();
            let count = backend.count(keyspace, range).await.unwrap();
            seen.push(format!(
                "{keyspace:?} {range:?} forward={forward:?} reverse={reverse:?} \
                 limited={limited:?} count={count}"
            ));
        }
        let firsts = backend.first_of_each(keyspace, ranges).await.unwrap();
        seen.push(format!("{keyspace:?} firsts={firsts:?}"));
    }
    seen
}

#[test]
fn a_random_write_stream_leaves_it_where_it_leaves_the_memory_backend() {
    let runtime = runtime();
    let root = tempfile::tempdir().unwrap();
    runtime.block_on(async {
        let engine = open(&root.path().join("differential")).await;
        let memory = MemoryBackend::new();
        let ranges = [
            KeyRange::all(),
            KeyRange::prefix(&[1]),
            KeyRange::between(key(&[1]), key(&[3, 2])),
            KeyRange::from_bounds(
                std::ops::Bound::Excluded(key(&[2])),
                std::ops::Bound::Included(key(&[4, 4])),
            ),
        ];
        let before = picture(&memory, &ranges).await;
        for seed in 1..=4_u64 {
            let mut stream = Stream(seed.wrapping_mul(0x9e37_79b9_7f4a_7c15));
            for _ in 0..400 {
                let mut batch = WriteBatch::new();
                for _ in 0..=stream.below(4) {
                    let keyspace = stream.keyspace();
                    batch = match stream.below(5) {
                        0 => batch.delete(keyspace, stream.key()),
                        1 => batch.expect_absent(keyspace, stream.key()),
                        _ => {
                            let written = stream.next().to_be_bytes();
                            batch.put(keyspace, stream.key(), value(&written))
                        }
                    };
                }
                let range = if stream.below(25) == 0 {
                    Some((
                        stream.keyspace(),
                        ranges[usize::try_from(stream.below(4)).unwrap()].clone(),
                    ))
                } else {
                    None
                };
                let landed_memory = AsyncKvBackend::apply(&memory, batch.clone()).await;
                let landed_engine = engine.apply(batch).await;
                assert_eq!(
                    landed_memory.is_ok(),
                    landed_engine.is_ok(),
                    "seed {seed}: the two judged one batch differently: \
                     memory {landed_memory:?}, engine {landed_engine:?}"
                );
                if let Some((keyspace, range)) = range {
                    AsyncKvBackend::delete_range(&memory, keyspace, &range)
                        .await
                        .unwrap();
                    engine.delete_range(keyspace, &range).await.unwrap();
                }
            }
            assert_eq!(
                picture(&engine, &ranges).await,
                picture(&memory, &ranges).await,
                "seed {seed}: the backends disagree after the same writes"
            );
        }
        // The control arm: the stream changed what both hold, so agreement is about content.
        assert_ne!(
            picture(&memory, &ranges).await,
            before,
            "the stream left the store as it found it"
        );
    });
}

#[test]
fn two_batches_each_guarding_the_key_the_other_writes_never_both_land() {
    let runtime = runtime();
    let root = tempfile::tempdir().unwrap();
    let backend = Arc::new(runtime.block_on(open(&root.path().join("skew"))));
    let rounds: u32 = 400;
    let mut both = 0_u32;
    for round in 0..rounds {
        let x = key(&[b'x', 0, 0]
            .iter()
            .copied()
            .chain(round.to_be_bytes())
            .collect::<Vec<_>>());
        let y = key(&[b'y', 0, 0]
            .iter()
            .copied()
            .chain(round.to_be_bytes())
            .collect::<Vec<_>>());
        // Write skew: each holds only while the key the other writes is absent.
        let first = WriteBatch::new()
            .expect_absent(Keyspace::DATA, x.clone())
            .put(Keyspace::DATA, y.clone(), value(b"1"));
        let second =
            WriteBatch::new()
                .expect_absent(Keyspace::DATA, y)
                .put(Keyspace::DATA, x, value(b"2"));
        let (one, two) = (Arc::clone(&backend), Arc::clone(&backend));
        let (landed_one, landed_two) = runtime.block_on(async move {
            let a = tokio::spawn(async move { one.apply(first).await });
            let b = tokio::spawn(async move { two.apply(second).await });
            (a.await.unwrap(), b.await.unwrap())
        });
        if landed_one.is_ok() && landed_two.is_ok() {
            both += 1;
        }
        assert!(
            landed_one.is_ok() || landed_two.is_ok(),
            "round {round}: neither landed: {landed_one:?} {landed_two:?}"
        );
    }
    assert_eq!(
        both, 0,
        "{both} of {rounds} rounds landed both batches of a write-skew pair"
    );
}

#[test]
fn a_store_reopens_with_what_it_held_and_its_key_is_required() {
    let runtime = runtime();
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("sealed");
    let at_rest =
        tessari_vault::AtRestKey::from_key(&tessari_vault::SecretBytes::adopt([7_u8; 32])).unwrap();
    runtime.block_on(async {
        let config = || StoreConfig::new(Durability::PowerLossSafe);
        let store = TessariKvBackend::open_with_key(&path, config(), Some(&at_rest))
            .await
            .unwrap();
        store
            .apply(WriteBatch::new().put(Keyspace::META, key(b"held"), value(b"yes")))
            .await
            .unwrap();
        store.close().await.unwrap();

        let refused = TessariKvBackend::open(&path, config()).await;
        assert!(
            matches!(&refused, Err(tessari_kv::Error::Validation { reason })
                if reason.contains("is encrypted; it opens only with its key")),
            "an encrypted store opened without its key: {refused:?}"
        );

        let reopened = TessariKvBackend::open_with_key(&path, config(), Some(&at_rest))
            .await
            .unwrap();
        assert_eq!(
            reopened.get(Keyspace::META, &key(b"held")).await.unwrap(),
            Some(value(b"yes"))
        );
        reopened.close().await.unwrap();
    });
}
