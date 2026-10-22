//! A snapshot written before topics' entries were carried still restores.

use std::sync::Arc;

use tessari_kv::{KvBackend, MemoryBackend};
use tessari_session::{RunNow, Session};
use tessari_storage::{Now, Store};

use super::{read_state, verify_state, write_state};
use crate::{Error, check};

fn store() -> Store {
    Store::open(Arc::new(MemoryBackend::new()) as Arc<dyn KvBackend>)
        .now()
        .expect("the store answers")
}

/// `file`, a format-2 snapshot carrying no entries, as format 1 wrote it: the
/// format byte, and an end frame counting chunks and records alone.
fn as_format_one(mut file: Vec<u8>) -> Vec<u8> {
    let end = file.len().saturating_sub(29);
    assert_eq!(file[end], super::FRAME_END);
    let counts_at = end.saturating_add(1);
    let counts = file[counts_at..counts_at.saturating_add(16)].to_vec();
    file.truncate(counts_at);
    file.extend_from_slice(&counts);
    file.extend_from_slice(&check::crc32(&counts).to_be_bytes());
    file[11] = 1;
    file
}

#[test]
fn a_format_one_snapshot_restores_as_it_always_did() {
    let source = store();
    Session::new(&source)
        .run_now(
            "DEFINE NAMESPACE n; USE NAMESPACE n; DEFINE DATABASE d; USE DATABASE d; \
             DEFINE TABLE kept SCHEMALESS; CREATE kept:1 = { x: 1 }; CREATE kept:2 = { x: 2 };",
        )
        .expect("the store answers");
    let mut file = Vec::new();
    let written = write_state(&source, &mut file)
        .now()
        .expect("the store answers");
    assert_eq!(written.entries, 0);
    let older = as_format_one(file);
    assert_eq!(
        verify_state(&mut older.as_slice())
            .expect("the store answers")
            .records,
        written.records
    );
    let target = store();
    read_state(&target, || Ok(older.as_slice()))
        .now()
        .expect("the store answers");
    let read = |store: &Store| {
        format!(
            "{:?}",
            Session::new(store)
                .run_now("USE NAMESPACE n; USE DATABASE d; SELECT * FROM kept;")
                .expect("the store answers")
        )
    };
    assert_eq!(read(&target), read(&source));
    assert!(read(&target).contains("Integer(2)"));
}

#[test]
fn a_snapshot_of_a_format_this_build_does_not_know_is_refused() {
    let mut file = Vec::new();
    write_state(&store(), &mut file)
        .now()
        .expect("the store answers");
    file[11] = 3;
    assert!(matches!(
        verify_state(&mut file.as_slice()),
        Err(Error::Unsupported {
            what: "snapshot format",
            found: 3,
            ..
        })
    ));
}
