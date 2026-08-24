//! Files, at the sizes a corpus case cannot reach.
//!
//! The corpus says what the statements mean. These say what happens at the seam
//! the corpus never crosses: a file larger than one chunk, a file written over a
//! longer one, and a bucket that has had files deleted from it — the three
//! places where "the bytes are records too" either holds or leaks.

#![allow(clippy::panic, clippy::unwrap_used, clippy::indexing_slicing)]

use std::sync::Arc;

use tessari_kv::{KvBackend, MemoryBackend};
use tessari_session::{Outcome, Parameters, Session};
use tessari_storage::Store;
use tessari_types::Value;

/// One more byte than two chunks hold, so the last chunk is a short one.
const CHUNK: usize = 1024 * 1024;

fn store() -> Store {
    let backend = Arc::new(MemoryBackend::new()) as Arc<dyn KvBackend>;
    Store::open(backend).unwrap()
}

fn ready(store: &Store) -> Session<'_> {
    let mut session = Session::new(store);
    session
        .run(
            "DEFINE NAMESPACE prod; USE NAMESPACE prod;\n\
             DEFINE DATABASE library; USE DATABASE library;\n\
             DEFINE BUCKET media;",
        )
        .unwrap();
    session
}

/// Bytes that are not all the same, so a chunk written in the wrong order or
/// twice is a difference this test can see.
fn pattern(len: usize) -> Vec<u8> {
    (0..len)
        .map(|at| {
            let held = at % 251;
            u8::try_from(held).unwrap_or(0)
        })
        .collect()
}

/// Write `bytes` to a path, through a parameter so the test does not build a
/// two-megabyte script.
fn put(session: &mut Session<'_>, path: &str, bytes: &[u8]) {
    let mut given = Parameters::new();
    given.insert("held".to_owned(), Value::Bytes(bytes.to_vec()));
    session
        .run_with(&format!("PUT media:'{path}' = $held;"), &given)
        .unwrap();
}

fn read(session: &mut Session<'_>, path: &str) -> Value {
    let outcomes = session.run(&format!("READ media:'{path}';")).unwrap();
    match &outcomes[0] {
        Outcome::Value(value) => value.clone(),
        other => panic!("not a value: {other:?}"),
    }
}

#[test]
fn a_file_larger_than_one_chunk_comes_back_byte_for_byte() {
    // The seam. One chunk is a record, which the corpus covers; three chunks is
    // the ordering, the ordinal encoding and the reassembly, none of which a
    // small file exercises at all.
    let store = store();
    let mut session = ready(&store);
    let bytes = pattern(CHUNK * 2 + 17);

    put(&mut session, "/big.bin", &bytes);
    assert_eq!(read(&mut session, "/big.bin"), Value::Bytes(bytes));
}

#[test]
fn the_metadata_says_what_the_store_actually_holds() {
    let store = store();
    let mut session = ready(&store);
    let bytes = pattern(CHUNK + 1);
    put(&mut session, "/big.bin", &bytes);

    let outcomes = session
        .run("SELECT size, chunks FROM media:'/big.bin';")
        .unwrap();
    let Outcome::Records { records, .. } = &outcomes[0] else {
        panic!("not records: {:?}", outcomes[0]);
    };
    let Value::Object(held) = &records[0].1 else {
        panic!("not an object");
    };
    assert_eq!(
        held.get("size"),
        Some(&Value::Number(tessari_types::Number::Integer(
            i64::try_from(CHUNK + 1).unwrap()
        )))
    );
    assert_eq!(
        held.get("chunks"),
        Some(&Value::Number(tessari_types::Number::Integer(2)))
    );
}

#[test]
fn a_shorter_file_written_over_a_longer_one_keeps_none_of_it() {
    // The leak this is looking for is silent and permanent: chunks past the new
    // file's count, described by nothing, read by nothing — until a later write
    // is long enough to reach them again and reads somebody else's bytes.
    let store = store();
    let mut session = ready(&store);
    put(&mut session, "/x.bin", &pattern(CHUNK * 3));

    let shorter = pattern(64);
    put(&mut session, "/x.bin", &shorter);
    assert_eq!(read(&mut session, "/x.bin"), Value::Bytes(shorter));

    // And the tail is gone rather than merely unreferenced: a file written long
    // again must not find the old chunks waiting.
    let longer = pattern(CHUNK * 3);
    put(&mut session, "/x.bin", &longer);
    assert_eq!(read(&mut session, "/x.bin"), Value::Bytes(longer));
}

#[test]
fn deleting_a_file_takes_its_bytes_with_it() {
    let store = store();
    let mut session = ready(&store);
    put(&mut session, "/gone.bin", &pattern(CHUNK + 5));
    session.run("DELETE media:'/gone.bin';").unwrap();

    assert_eq!(read(&mut session, "/gone.bin"), Value::None);

    // Written again at the same path, the file is what was written now — not
    // what was written before, and not the two interleaved.
    let again = pattern(32);
    put(&mut session, "/gone.bin", &again);
    assert_eq!(read(&mut session, "/gone.bin"), Value::Bytes(again));
}

#[test]
fn two_files_in_one_bucket_do_not_share_bytes() {
    // The chunk identity is the path followed by the ordinal. If the two halves
    // were ever confusable — a path that is a prefix of another, say — this is
    // where it shows.
    let store = store();
    let mut session = ready(&store);
    let first = pattern(CHUNK + 3);
    let second = pattern(CHUNK + 9);

    put(&mut session, "/a", &first);
    put(&mut session, "/a.bin", &second);

    assert_eq!(read(&mut session, "/a"), Value::Bytes(first));
    assert_eq!(read(&mut session, "/a.bin"), Value::Bytes(second));
}

#[test]
fn an_empty_file_is_a_file() {
    let store = store();
    let mut session = ready(&store);
    put(&mut session, "/empty", &[]);

    assert_eq!(read(&mut session, "/empty"), Value::Bytes(Vec::new()));
    let outcomes = session.run("SELECT * FROM media;").unwrap();
    let Outcome::Records { records, .. } = &outcomes[0] else {
        panic!("not records: {:?}", outcomes[0]);
    };
    assert_eq!(records.len(), 1, "an empty file is still a file");
}
