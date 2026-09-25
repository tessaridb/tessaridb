//! `KEYS … PREFIX | RANGE … AFTER … LIMIT` (G035 S4.1).

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::thread;

use tessari_kv::{
    Key, Keyspace, KvBackend, Result as KvResult, ScanRequest, Value as KvValue, WriteBatch,
};
use tessari_session::{Outcome, Session};
use tessari_storage::Store;
use tessari_types::RecordId;

use super::{Backend, PAST_SHORT, SHORT, on_each_backend, opened, run};

fn listed(session: &mut Session<'_>, script: &str) -> Vec<RecordId> {
    match run(session, script) {
        Outcome::Keys(keys) => keys,
        other => panic!("{script} answered {other:?}"),
    }
}

fn texts(ids: &[&str]) -> Vec<RecordId> {
    ids.iter().map(|id| RecordId::from(*id)).collect()
}

#[test]
fn a_prefix_lists_exactly_the_keys_that_begin_with_it() {
    on_each_backend(|backend| {
        let mut session = opened(&backend.store);
        for key in [
            "user:42",
            "user:42:a",
            "user:42:b",
            "user:42;",
            "user:420",
            "user:43:x",
            "user:41:z",
        ] {
            run(&mut session, &format!("SET cache:'{key}' = 1;"));
        }
        run(&mut session, "SET cache:7 = 1;");
        assert_eq!(
            listed(&mut session, "KEYS FROM cache PREFIX 'user:42:';"),
            texts(&["user:42:a", "user:42:b"]),
            "{}",
            backend.name
        );
        assert_eq!(
            listed(&mut session, "KEYS FROM cache PREFIX 'user:42';"),
            // Byte order: `0` (0x30) sorts before `:` (0x3a) and `;` (0x3b).
            texts(&["user:42", "user:420", "user:42:a", "user:42:b", "user:42;"]),
            "{}",
            backend.name
        );
    });
}

#[test]
fn after_and_limit_page_through_every_key_once() {
    on_each_backend(|backend| {
        let mut session = opened(&backend.store);
        let all: Vec<String> = (0..10).map(|n| format!("k{n:02}")).collect();
        for key in &all {
            run(&mut session, &format!("SET cache:'{key}' = 1;"));
        }
        let mut seen = Vec::new();
        let mut after: Option<String> = None;
        loop {
            let page = match &after {
                Some(last) => listed(
                    &mut session,
                    &format!("KEYS FROM cache AFTER '{last}' LIMIT 3;"),
                ),
                None => listed(&mut session, "KEYS FROM cache LIMIT 3;"),
            };
            assert!(
                page.len() <= 3,
                "{}: a page is at most its limit",
                backend.name
            );
            let Some(RecordId::Text(last)) = page.last().cloned() else {
                break;
            };
            seen.extend(page);
            after = Some(last);
        }
        assert_eq!(
            seen,
            all.iter()
                .map(|key| RecordId::from(key.as_str()))
                .collect::<Vec<_>>(),
            "{}: no key repeated, none dropped",
            backend.name
        );
        assert_eq!(
            listed(
                &mut session,
                "KEYS FROM cache PREFIX 'k0' AFTER 'k03' LIMIT 2;"
            ),
            texts(&["k04", "k05"]),
            "{}",
            backend.name
        );
    });
}

#[test]
fn an_expired_key_is_not_listed() {
    on_each_backend(|backend| {
        let mut session = opened(&backend.store);
        run(
            &mut session,
            &format!("SET cache:'p:1' = 1 EXPIRE {SHORT}; SET cache:'p:2' = 1;"),
        );
        assert_eq!(
            listed(&mut session, "KEYS FROM cache PREFIX 'p:';").len(),
            2
        );
        thread::sleep(PAST_SHORT);
        assert_eq!(
            listed(&mut session, "KEYS FROM cache PREFIX 'p:';"),
            texts(&["p:2"]),
            "{}",
            backend.name
        );
    });
}

/// A backend that answers as the one beneath it and counts the entries it hands
/// back.
#[derive(Debug)]
struct Counting {
    inner: Arc<dyn KvBackend>,
    entries: AtomicUsize,
}

impl KvBackend for Counting {
    fn name(&self) -> &'static str {
        "counting"
    }
    fn get(&self, keyspace: Keyspace, key: &Key) -> KvResult<Option<KvValue>> {
        self.inner.get(keyspace, key)
    }
    fn scan(&self, request: &ScanRequest) -> KvResult<Vec<(Key, KvValue)>> {
        let found = self.inner.scan(request)?;
        self.entries.fetch_add(found.len(), Ordering::Relaxed);
        Ok(found)
    }
    fn apply(&self, batch: WriteBatch) -> KvResult<()> {
        self.inner.apply(batch)
    }
}

/// The seek, measured: a prefix over three keys in a space of a thousand reads
/// a handful of entries, where a walk of the space would read every one.
#[test]
fn a_prefix_walk_reads_its_stretch_and_not_the_space() {
    on_each_backend(|backend: &Backend| {
        let counting = Arc::new(Counting {
            inner: Arc::clone(&backend.raw),
            entries: AtomicUsize::new(0),
        });
        let store = Store::open(Arc::clone(&counting) as Arc<dyn KvBackend>).unwrap();
        let mut session = opened(&store);
        let mut script = String::new();
        for n in 0..1_000 {
            script.push_str(&format!("SET cache:'a:{n:04}' = 1;"));
        }
        script.push_str("SET cache:'z:1' = 1; SET cache:'z:2' = 1; SET cache:'z:3' = 1;");
        run(&mut session, &script);

        counting.entries.store(0, Ordering::Relaxed);
        let found = listed(&mut session, "KEYS FROM cache PREFIX 'z:';");
        let read = counting.entries.load(Ordering::Relaxed);
        assert_eq!(found, texts(&["z:1", "z:2", "z:3"]), "{}", backend.name);
        assert!(
            read < 100,
            "{}: the walk read {read} entries for a stretch of three",
            backend.name
        );
    });
}
