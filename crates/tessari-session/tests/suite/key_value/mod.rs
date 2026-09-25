//! The key-value verbs a cache needs (G035), on both backends.
//!
//! Every case runs twice — on the memory backend and on the disk one — because
//! the owner's condition for this feature is that a space behaves the same in
//! either, and a feature tested on one backend is a claim about one backend.

mod atomic;
mod bounded;
mod expiry;
mod removal;
mod walk;

use std::sync::Arc;
use std::time::Duration;

use tessari_kv::{KvBackend, MemoryBackend};
use tessari_lsm::{Durability, LsmBackend, StoreConfig};
use tessari_session::{Outcome, Session};
use tessari_storage::Store;
use tessari_types::Value;

/// How long a short expiry lasts in these tests, and how long a test waits past
/// it. Real time rather than an injected clock: the clock is the transaction's
/// own, and a test that replaced it would be testing the replacement.
pub(super) const SHORT: &str = "300ms";
pub(super) const PAST_SHORT: Duration = Duration::from_millis(450);

/// One store on one backend, with the directory it lives in when on disk.
pub(super) struct Backend {
    pub(super) name: &'static str,
    pub(super) raw: Arc<dyn KvBackend>,
    pub(super) store: Store,
    _directory: Option<tempfile::TempDir>,
}

/// Run `case` against a fresh store on each backend in turn.
pub(super) fn on_each_backend(case: impl Fn(&Backend)) {
    let raw = Arc::new(MemoryBackend::new()) as Arc<dyn KvBackend>;
    case(&Backend {
        name: "memory",
        store: Store::open(Arc::clone(&raw)).unwrap(),
        raw,
        _directory: None,
    });
    let directory = tempfile::tempdir().unwrap();
    let raw = Arc::new(
        LsmBackend::open(
            directory.path(),
            StoreConfig::new(Durability::ProcessCrashSafe),
        )
        .unwrap(),
    ) as Arc<dyn KvBackend>;
    case(&Backend {
        name: "disk",
        store: Store::open(Arc::clone(&raw)).unwrap(),
        raw,
        _directory: Some(directory),
    });
}

/// A session in `prod`/`app` with a space `cache` declared.
pub(super) fn opened(store: &Store) -> Session<'_> {
    let mut session = Session::new(store);
    session
        .run(
            "DEFINE NAMESPACE prod; USE NAMESPACE prod;\n\
             DEFINE DATABASE app; USE DATABASE app;\n\
             DEFINE SPACE cache;",
        )
        .unwrap();
    session
}

/// The last statement's outcome.
pub(super) fn run(session: &mut Session<'_>, script: &str) -> Outcome {
    session.run(script).unwrap().pop().unwrap()
}

/// The value a statement answers with.
pub(super) fn value(session: &mut Session<'_>, script: &str) -> Value {
    match run(session, script) {
        Outcome::Value(value) => value,
        other => panic!("{script} answered {other:?}"),
    }
}

/// How many records a read answers with.
pub(super) fn rows(session: &mut Session<'_>, script: &str) -> usize {
    match run(session, script) {
        Outcome::Records { records, .. } => records.len(),
        other => panic!("{script} answered {other:?}"),
    }
}

/// How many keys a `KEYS` answers with.
pub(super) fn keys(session: &mut Session<'_>, script: &str) -> usize {
    match run(session, script) {
        Outcome::Keys(keys) => keys.len(),
        other => panic!("{script} answered {other:?}"),
    }
}

/// The refusal a script meets, as text.
pub(super) fn refused(session: &mut Session<'_>, script: &str) -> String {
    match session.run(script) {
        Err(why) => why.to_string(),
        Ok(outcome) => panic!("expected {script} to be refused, got {outcome:?}"),
    }
}
