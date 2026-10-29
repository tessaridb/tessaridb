//! A present read whose snapshot another connection's commit passed (DB-V15).
//!
//! An index describes the committed tail, so a read whose snapshot is older
//! cannot believe one, and a traversal, which has no scan to fall back to, is
//! refused. That refusal was written for `VERSION`, a read of the past someone
//! asked for, and it was also what a present read got when a commit landed
//! between its snapshot and its walk: a statement that asked about now was told
//! it cannot be read at an earlier version, in the class that says repeating it
//! cannot succeed.
//!
//! Two answers now, by who can act on it. A statement running alone has
//! committed nothing, so the store runs it again on a fresh snapshot, as it
//! already does for a lone atomic key-value write. A statement inside `BEGIN`
//! cannot be run again alone, so it is refused in the `retry` class and the
//! caller runs the transaction again. A `VERSION` read keeps its refusal.

use std::sync::Arc;
use tessari_session::Now;
use tessari_session::RunNow;

use tessari_kv::{KvBackend, MemoryBackend};
use tessari_session::{Error, Session};
use tessari_storage::Store;
use tessari_types::RefusalClass;

const SCHEMA: &str = "\
DEFINE NAMESPACE prod; USE NAMESPACE prod;
DEFINE DATABASE social; USE DATABASE social;
DEFINE COLLECTION users;
DEFINE TABLE follows EDGE;
DEFINE INDEX by_handle ON users FIELDS handle UNIQUE;
CREATE users:1 = { handle: 'ada' };
CREATE users:2 = { handle: 'grace' };
RELATE users:1->follows->users:2;";

const USE: &str = "USE NAMESPACE prod; USE DATABASE social;";

fn store() -> Store {
    let store = Store::open(Arc::new(MemoryBackend::new()) as Arc<dyn KvBackend>)
        .now()
        .unwrap();
    Session::new(&store).run_now(SCHEMA).unwrap();
    store
}

/// Reads run in transactions of their own beside writers on other connections;
/// the refusals each read met, as class and text.
///
/// `ROUNDS` is set so the 0.39.3 engine refused present reads in every run
/// measured (DB-V15: 321 of 1 200 lone reads in the first one).
fn race(reads: &'static [&'static str]) -> Vec<(RefusalClass, String)> {
    const WRITERS: u64 = 4;
    const READERS: u64 = 4;
    const ROUNDS: u64 = 150;
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(4)
        .enable_all()
        .build()
        .unwrap();
    let store: &'static Store = Box::leak(Box::new(store()));
    runtime.block_on(async move {
        let mut running = tokio::task::JoinSet::new();
        for writer in 0..WRITERS {
            running.spawn(async move {
                let mut session = Session::new(store);
                session.run(USE).await.unwrap();
                for round in 0..ROUNDS {
                    let id = format!("w{writer}r{round}");
                    session
                        .run(&format!("CREATE users:'{id}' = {{ handle: '{id}' }};"))
                        .await
                        .unwrap();
                }
                Vec::new()
            });
        }
        for _ in 0..READERS {
            running.spawn(async move {
                let mut session = Session::new(store);
                session.run(USE).await.unwrap();
                let mut refused = Vec::new();
                for _ in 0..ROUNDS {
                    for read in reads {
                        if let Err(refusal) = session.run(read).await {
                            refused.push((refusal.class(), refusal.to_string()));
                        }
                    }
                }
                refused
            });
        }
        let mut refused = Vec::new();
        while let Some(done) = running.join_next().await {
            refused.extend(done.unwrap());
        }
        refused
    })
}

#[test]
fn a_traversal_inside_a_transaction_a_commit_passed_is_refused_as_retry() {
    let refused = race(&["BEGIN; SELECT * FROM users:1->follows->users; COMMIT;"]);
    // The control arm: the race happened, or the assertion below asserts nothing.
    assert!(
        !refused.is_empty(),
        "no transaction fell behind; raise ROUNDS"
    );
    for (class, text) in &refused {
        assert_eq!(*class, RefusalClass::Retry, "{text}");
        assert!(
            text.contains("another commit"),
            "the refusal says why: {text}"
        );
    }
}

#[test]
fn a_traversal_at_a_version_keeps_the_historical_refusal() {
    let store = store();
    let mut session = Session::new(&store);
    session.run_now(USE).unwrap();
    let before = store.committed_version().now().unwrap().get();
    session
        .run_now("CREATE users:3 = { handle: 'katherine' };")
        .unwrap();
    let refusal = session
        .run_now(&format!(
            "SELECT * FROM users:1->follows->users VERSION {before};"
        ))
        .unwrap_err();
    assert!(
        matches!(refusal, Error::NoHistoricalTraversal { .. }),
        "{refusal:?}"
    );
    assert_eq!(refusal.class(), RefusalClass::Invalid);
}

/// Lone present reads beside writers on other connections: every one answers,
/// because a statement that committed nothing is run again on a fresh snapshot.
#[test]
fn lone_present_reads_beside_concurrent_commits_all_answer() {
    let refused = race(&[
        "SELECT * FROM users:1->follows->users;",
        "SELECT * FROM users WHERE handle = 'ada' USING INDEX by_handle;",
    ]);
    assert!(
        refused.is_empty(),
        "{} present reads refused, first: {:?}",
        refused.len(),
        refused.first()
    );
}
