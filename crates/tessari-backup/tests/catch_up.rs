//! A follower that was stopped comes back and catches up.
//!
//! `bootstrap.rs` already converges a follower across a gap, so it is tempting to
//! read criterion G3 as satisfied there. It is not, and the gap is exactly one
//! property: that file holds `follow_from` in a **local variable** for the whole
//! outage. A follower that was really stopped has no such variable — it died with
//! the process. So the question this file asks is the one `bootstrap.rs` never
//! does: is the position **recoverable**, or was it merely remembered?
//!
//! The nearest existing answer is `tessari-lsm`'s
//! `a_commit_after_a_reopen_continues_the_sequence_instead_of_restarting_it`,
//! which shows the next sequence is *greater* than the old tail. Greater is not
//! equal, and neither of them is about following. Hence this file — and hence a
//! durable backend, because a reopen on a shared in-process handle would exercise
//! the store's recovery path without ever leaving the process that remembered.

#![allow(clippy::panic, clippy::unwrap_used, clippy::indexing_slicing)]

use std::path::Path;
use std::sync::Arc;

use tessari_kv::{KvBackend, MemoryBackend};
use tessari_lsm::{Durability, LsmBackend, StoreConfig};
use tessari_session::Session;
use tessari_storage::Store;
use tessari_types::Sequence;

/// The same fixture `bootstrap.rs` uses, so the two files' sequences stay
/// comparable when one of them moves.
///
/// Duplicated rather than shared: each file under `tests/` is its own crate, so
/// the alternative is a `tests/support/mod.rs` — which would mean editing
/// `bootstrap.rs` in a wave that is not about `bootstrap.rs`. The extraction is
/// already owed elsewhere (wave 56, `tessari-http`) and belongs in that wave.
const LEADER: &str = "\
DEFINE NAMESPACE prod; USE NAMESPACE prod;\n\
DEFINE DATABASE orders; USE DATABASE orders;\n\
DEFINE TABLE people;\n\
DEFINE FIELD name ON people TYPE string;\n\
DEFINE INDEX by_email ON people FIELDS email UNIQUE;\n\
CREATE people:1 = { name: 'ada', email: 'a@x' };\n\
CREATE people:2 = { name: 'grace', email: 'b@x' };\n\
DELETE people:2;\n\
CREATE people:3 = { name: 'edith', email: 'c@x' };";

/// Reads that would answer differently if anything had been missed.
const INTERROGATION: &[&str] = &[
    "SELECT * FROM people;",
    "SELECT * FROM people WHERE email = 'a@x';",
    "SELECT * FROM people WHERE name > 'b';",
    "INFO FOR TABLE people;",
];

/// The follower's store, on disk, so that dropping it is a real shutdown.
///
/// Nothing keeps a second `Arc`: the `Store` holds the only one, so when it goes
/// the backend goes with it and the directory is released. That is the whole
/// point of the helper, and holding a spare handle here would quietly turn every
/// test in this file back into `bootstrap.rs`.
fn durable(path: &Path) -> Store {
    let backend = LsmBackend::open(path, StoreConfig::new(Durability::ProcessCrashSafe)).unwrap();
    Store::open(Arc::new(backend) as Arc<dyn KvBackend>).unwrap()
}

/// The leader is not the subject here, so it stays in memory.
fn in_memory() -> Store {
    Store::open(Arc::new(MemoryBackend::new()) as Arc<dyn KvBackend>).unwrap()
}

fn session(store: &Store) -> Session<'_> {
    let mut session = Session::new(store);
    session
        .run("USE NAMESPACE prod; USE DATABASE orders;")
        .unwrap();
    session
}

/// A leader holding records, and the whole prefix of its log.
fn leader() -> (Store, Vec<u8>) {
    let store = in_memory();
    {
        let mut opening = Session::new(&store);
        opening.run(LEADER).unwrap();
    }
    let mut prefix = Vec::new();
    let written = tessari_backup::write(&store, &mut prefix).unwrap();
    assert!(written.records > 0);
    (store, prefix)
}

/// What both stores say to the same questions.
fn interrogate(store: &Store) -> Vec<String> {
    let mut session = session(store);
    INTERROGATION
        .iter()
        .map(|script| format!("{:?}", session.run(script).unwrap()))
        .collect()
}

/// Where a follower would resume, computed from nothing but the store itself.
fn resume_from(store: &Store) -> Sequence {
    Sequence::new(store.committed_tail().unwrap().get().saturating_add(1))
}

#[test]
fn a_follower_that_was_stopped_recovers_its_position_from_its_own_store() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("follower");

    let (_leader, prefix) = leader();

    let reported = {
        let follower = durable(&path);
        let brought_up = tessari_backup::bootstrap(&follower, &mut prefix.as_slice()).unwrap();
        assert!(brought_up.records > 0, "a bootstrap that applied nothing");
        brought_up.follow_from
        // `follower` drops here: last `Arc` gone, backend closed, directory
        // released. Everything the process knew about the position is now gone
        // too, which is the precondition the assertion below depends on.
    };

    let reopened = durable(&path);
    assert_eq!(
        resume_from(&reopened),
        reported,
        "the position did not survive the restart, so it was remembered and not stored"
    );
}

#[test]
fn a_follower_that_was_down_catches_up_on_what_it_missed_without_replaying() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("follower");

    let (held, prefix) = leader();

    {
        let follower = durable(&path);
        tessari_backup::bootstrap(&follower, &mut prefix.as_slice()).unwrap();
    }
    // The follower is down from here. Note that no position was carried out of
    // that block — deliberately, because carrying one is what would make this
    // test pass for `bootstrap.rs`'s weaker reason.

    // The leader moves on while nobody is listening, in both of the ways it can:
    // a schema change and a record change. The schema half is the one a change
    // feed would have dropped.
    {
        let mut moving = session(&held);
        moving
            .run("DEFINE FIELD city ON people TYPE string;")
            .unwrap();
        moving
            .run("CREATE people:4 = { name: 'katherine', email: 'd@x', city: 'hampton' };")
            .unwrap();
    }

    let reopened = durable(&path);

    let mut missed = Vec::new();
    let sent = tessari_backup::write_from(&held, &mut missed, resume_from(&reopened)).unwrap();
    assert!(
        sent.records > 0,
        "the leader had nothing to send, so this test proves nothing"
    );

    let caught_up = tessari_backup::bootstrap(&reopened, &mut missed.as_slice()).unwrap();

    // Nothing replayed: what arrived is exactly what was written during the
    // outage, not that plus the history again.
    assert_eq!(caught_up.records, sent.records);
    // Nothing skipped: the two agree again, including about the field that only
    // the catalog knows.
    assert_eq!(interrogate(&held), interrogate(&reopened));
    assert_eq!(caught_up.follow_from, resume_from(&held));
}
