//! Part of a file, read and written.
//!
//! The §8 reason for leaving this out assumed a partial write would be several
//! commits — "a partial write needs a rule for what a reader sees between two of
//! them". It does not have to be. A ranged write that lands in **one** commit
//! has no between, so the question dissolves rather than being answered.
//!
//! What that leaves absent is a *staged* upload: many commits building one file,
//! which does need such a rule.
//!
//! Every ranged-write test asserts the **whole** file afterwards rather than the
//! range it wrote, because the way a splice fails is by losing the bytes nobody
//! was looking at.

#![allow(clippy::panic, clippy::unwrap_used, clippy::indexing_slicing)]

use std::sync::Arc;

use tessari_kv::{KvBackend, MemoryBackend};
use tessari_session::{Outcome, Session};
use tessari_storage::Store;
use tessari_types::Value;

/// The chunk size the file module uses, so the tests can straddle a boundary
/// deliberately rather than by luck.
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
             DEFINE DATABASE work; USE DATABASE work;\n\
             DEFINE BUCKET media;",
        )
        .unwrap();
    session
}

/// A file of `size` bytes whose content is a function of the offset, so a
/// misplaced splice shows as a wrong *value* and not only as a wrong length.
fn seeded(size: usize) -> Vec<u8> {
    (0..size)
        .map(|at| u8::try_from(at % 251).unwrap_or(0))
        .collect()
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn read(session: &mut Session<'_>, script: &str) -> Vec<u8> {
    let outcomes = session.run(script).unwrap();
    match outcomes.last() {
        Some(Outcome::Value(Value::Bytes(held))) => held.clone(),
        other => panic!("a read answered with {other:?}"),
    }
}

/// Write `bytes` as the whole file at `/big.bin`.
fn write_whole(session: &mut Session<'_>, bytes: &[u8]) {
    session
        .run(&format!("PUT media:'/big.bin' = 0x{};", hex(bytes)))
        .unwrap();
}

#[test]
fn a_range_within_one_chunk_answers_exactly_those_bytes() {
    let store = store();
    let mut session = ready(&store);
    let held = seeded(4096);
    write_whole(&mut session, &held);
    assert_eq!(
        read(&mut session, "READ media:'/big.bin' START 100 LIMIT 50;"),
        held[100..150]
    );
}

#[test]
fn a_range_across_a_chunk_boundary_answers_exactly_those_bytes() {
    // The case a per-chunk reader gets wrong: the range begins in one chunk and
    // ends in the next, so both a start offset and an end offset are in play.
    let store = store();
    let mut session = ready(&store);
    let held = seeded(CHUNK + 4096);
    write_whole(&mut session, &held);
    let from = CHUNK - 100;
    assert_eq!(
        read(
            &mut session,
            &format!("READ media:'/big.bin' START {from} LIMIT 300;")
        ),
        held[from..from + 300]
    );
}

#[test]
fn the_bounds_are_optional_and_mean_what_they_mean_over_rows() {
    let store = store();
    let mut session = ready(&store);
    let held = seeded(5000);
    write_whole(&mut session, &held);
    // Neither: the whole file, which is what it always was.
    assert_eq!(read(&mut session, "READ media:'/big.bin';"), held);
    // Only a start: to the end.
    assert_eq!(
        read(&mut session, "READ media:'/big.bin' START 4990;"),
        held[4990..]
    );
    // Only a limit: from the beginning.
    assert_eq!(
        read(&mut session, "READ media:'/big.bin' LIMIT 10;"),
        held[..10]
    );
}

#[test]
fn a_range_that_finds_nothing_is_empty_rather_than_a_failure() {
    // The rule `START` past the last row already follows.
    let store = store();
    let mut session = ready(&store);
    write_whole(&mut session, &seeded(100));
    assert!(read(&mut session, "READ media:'/big.bin' START 500;").is_empty());
    assert!(read(&mut session, "READ media:'/big.bin' START 100;").is_empty());
    assert!(read(&mut session, "READ media:'/big.bin' LIMIT 0;").is_empty());
    // …and a limit past the end answers what is there.
    assert_eq!(
        read(&mut session, "READ media:'/big.bin' START 90 LIMIT 999;").len(),
        10
    );
}

#[test]
fn a_ranged_write_replaces_those_bytes_and_leaves_the_rest() {
    let store = store();
    let mut session = ready(&store);
    let held = seeded(4096);
    write_whole(&mut session, &held);
    session
        .run("PUT media:'/big.bin' START 100 = 0xdeadbeef;")
        .unwrap();

    let mut expected = held.clone();
    expected[100..104].copy_from_slice(&[0xde, 0xad, 0xbe, 0xef]);
    // The whole file, because a bad splice loses the bytes nobody looked at.
    assert_eq!(read(&mut session, "READ media:'/big.bin';"), expected);
}

#[test]
fn a_ranged_write_across_a_chunk_boundary_keeps_both_sides() {
    let store = store();
    let mut session = ready(&store);
    let held = seeded(CHUNK + 4096);
    write_whole(&mut session, &held);
    let from = CHUNK - 2;
    let written = [1_u8, 2, 3, 4];
    session
        .run(&format!(
            "PUT media:'/big.bin' START {from} = 0x{};",
            hex(&written)
        ))
        .unwrap();

    let mut expected = held.clone();
    expected[from..from + 4].copy_from_slice(&written);
    let found = read(&mut session, "READ media:'/big.bin';");
    assert_eq!(found.len(), expected.len());
    assert_eq!(found, expected);
}

#[test]
fn a_ranged_write_that_runs_past_the_end_grows_the_file() {
    let store = store();
    let mut session = ready(&store);
    let held = seeded(100);
    write_whole(&mut session, &held);
    session
        .run("PUT media:'/big.bin' START 98 = 0xaabbccdd;")
        .unwrap();

    let found = read(&mut session, "READ media:'/big.bin';");
    assert_eq!(found.len(), 102, "the file did not grow");
    assert_eq!(found[..98], held[..98]);
    assert_eq!(found[98..], [0xaa, 0xbb, 0xcc, 0xdd]);
    // …and the metadata kept up, which is what a read trusts for the chunk count.
    let outcomes = session.run("SELECT * FROM media:'/big.bin';").unwrap();
    let records = outcomes.last().unwrap().records().unwrap();
    let Value::Object(fields) = &records[0].1 else {
        panic!("not an object");
    };
    assert_eq!(
        format!("{:?}", fields.get("size").unwrap()),
        "Number(Integer(102))"
    );
}

#[test]
fn a_write_that_would_leave_a_hole_is_refused_and_changes_nothing() {
    // Zero-filling would be the store inventing bytes nobody wrote, and a real
    // hole is a sparse-file feature nobody asked for.
    let store = store();
    let mut session = ready(&store);
    let held = seeded(100);
    write_whole(&mut session, &held);

    let refused = session.run("PUT media:'/big.bin' START 200 = 0xff;");
    assert!(refused.is_err(), "{refused:?}");
    assert_eq!(
        read(&mut session, "READ media:'/big.bin';"),
        held,
        "a refused write changed the file"
    );

    // The same rule on a file that does not exist yet: only offset zero is a
    // beginning.
    let refused = session.run("PUT media:'/new.bin' START 8 = 0xff;");
    assert!(refused.is_err(), "{refused:?}");
    session.run("PUT media:'/new.bin' START 0 = 0xff;").unwrap();
    assert_eq!(read(&mut session, "READ media:'/new.bin';"), vec![0xff]);
}

#[test]
fn start_zero_writes_at_the_beginning_rather_than_replacing_the_file() {
    // One spelling per thing: leaving `START` out replaces the file, and giving
    // an offset — any offset, including zero — writes at it.
    let store = store();
    let mut session = ready(&store);
    let held = seeded(50);
    write_whole(&mut session, &held);
    session
        .run("PUT media:'/big.bin' START 0 = 0x0102;")
        .unwrap();

    let found = read(&mut session, "READ media:'/big.bin';");
    assert_eq!(found.len(), 50, "a ranged write truncated the file");
    assert_eq!(found[..2], [1, 2]);
    assert_eq!(found[2..], held[2..]);

    // …while the whole-file form still truncates, which is what it is for.
    write_whole(&mut session, &[9, 9]);
    assert_eq!(read(&mut session, "READ media:'/big.bin';"), vec![9, 9]);
}

#[test]
fn a_ranged_read_of_a_file_that_is_not_there_is_still_none() {
    let store = store();
    let mut session = ready(&store);
    let outcomes = session
        .run("READ media:'/missing.bin' START 10 LIMIT 10;")
        .unwrap();
    assert!(matches!(outcomes.last(), Some(Outcome::Value(Value::None))));
}
