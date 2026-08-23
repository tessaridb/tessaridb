//! The build that wrote a backup, and why the check is asymmetric.
//!
//! A backup file already carried two versions: the framing's, which says how to
//! find the records, and the codec's, which says how to decode one. Neither says
//! what the build that produced them **meant**, and that is a third question —
//! a newer build can write byte-identical framing and perfectly decodable
//! records while having changed what one of them does.
//!
//! So the header names the writer, and the two directions are treated
//! differently on purpose:
//!
//! - **Older into newer is allowed and reported.** That is the ordinary case,
//!   and making it visible is the whole reason to record a version.
//! - **Newer into older is refused.** The same reasoning that refuses a newer
//!   on-disk format: guessing at what a newer build meant produces records
//!   nobody wrote, and it does it silently.

#![allow(clippy::panic, clippy::unwrap_used, clippy::indexing_slicing)]

use std::sync::Arc;

use bgv_db_encoding::NodeVersion;
use bgv_db_kv::{KvBackend, MemoryBackend};
use bgv_db_session::Session;
use bgv_db_storage::Store;

/// Where the three writer numbers sit: magic (8) + format (1) + codec (1).
const WRITER_AT: usize = 10;

fn store() -> (Arc<dyn KvBackend>, Store) {
    let backend = Arc::new(MemoryBackend::new()) as Arc<dyn KvBackend>;
    let store = Store::open(Arc::clone(&backend)).unwrap();
    (backend, store)
}

/// A store with something in it, and its backup.
fn taken() -> Vec<u8> {
    let (_, store) = store();
    {
        let mut session = Session::new(&store);
        session
            .run(
                "DEFINE NAMESPACE prod; USE NAMESPACE prod;\n\
                 DEFINE DATABASE shop; USE DATABASE shop;\n\
                 DEFINE TABLE orders;\n\
                 CREATE orders:1 = { total: 3 };",
            )
            .unwrap();
    }
    let mut bytes = Vec::new();
    let written = bgv_db_backup::write(&store, &mut bytes).unwrap();
    assert_eq!(written.writer, NodeVersion::current());
    assert!(written.records > 0);
    bytes
}

/// The same file, claiming to have been written by `version`.
fn claiming(mut bytes: Vec<u8>, version: NodeVersion) -> Vec<u8> {
    bytes[WRITER_AT..WRITER_AT + 4].copy_from_slice(&version.major.to_be_bytes());
    bytes[WRITER_AT + 4..WRITER_AT + 8].copy_from_slice(&version.minor.to_be_bytes());
    bytes[WRITER_AT + 8..WRITER_AT + 12].copy_from_slice(&version.patch.to_be_bytes());
    bytes
}

#[test]
fn a_backup_names_the_build_that_wrote_it() {
    let bytes = taken();
    let checked = bgv_db_backup::verify(&mut bytes.as_slice()).unwrap();
    assert_eq!(checked.written_by, NodeVersion::current());
}

#[test]
fn a_restore_reports_the_build_the_file_came_from() {
    let bytes = taken();
    let (_, into) = store();
    let restored = bgv_db_backup::read(&into, &mut bytes.as_slice()).unwrap();
    assert_eq!(restored.written_by, NodeVersion::current());
    assert!(restored.records > 0);
}

#[test]
fn a_backup_from_an_older_build_restores_and_says_where_it_came_from() {
    // The case the field exists for. An older backup going into a newer binary
    // is the ordinary upgrade path, so it must restore — and it must be
    // reported, or the version would be recorded and never seen.
    let older = NodeVersion {
        major: 0,
        minor: 0,
        patch: 0,
    };
    assert!(older <= NodeVersion::current(), "the fixture is not older");
    let bytes = claiming(taken(), older);

    let (_, into) = store();
    let restored = bgv_db_backup::read(&into, &mut bytes.as_slice()).unwrap();
    assert_eq!(restored.written_by, older);
    assert!(restored.records > 0, "an older backup must still restore");
}

#[test]
fn a_backup_from_a_newer_build_is_refused_before_anything_is_applied() {
    let running = NodeVersion::current();
    let newer = NodeVersion {
        major: running.major.saturating_add(1),
        ..running
    };
    let bytes = claiming(taken(), newer);

    let (_, into) = store();
    let refused = bgv_db_backup::read(&into, &mut bytes.as_slice()).unwrap_err();
    let said = refused.to_string();
    assert!(said.contains(&newer.to_string()), "{said}");
    assert!(said.contains(&running.to_string()), "{said}");

    // **Before anything is applied**, which is the part that matters: a
    // half-applied restore is worse than a refused one, and the header is read
    // whole before the first record is looked at.
    assert_eq!(into.committed_tail().unwrap().get(), 0);
}

#[test]
fn a_file_from_the_previous_header_layout_is_refused_by_name() {
    // The format byte moved 2 -> 3 when the header gained the writer. A
    // version-2 file has twelve fewer bytes before its bounds, so reading it as
    // a version-3 one would take the writer's numbers out of the bounds and
    // silently restore from the wrong sequence. It is refused by the byte
    // instead, which is what that byte is for.
    let mut bytes = taken();
    bytes[8] = 2;
    let (_, into) = store();
    let refused = bgv_db_backup::read(&into, &mut bytes.as_slice())
        .unwrap_err()
        .to_string();
    assert!(refused.contains("format"), "{refused}");
    assert!(refused.contains('2'), "{refused}");
}
