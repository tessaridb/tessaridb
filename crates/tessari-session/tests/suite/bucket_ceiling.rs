//! `DEFINE BUCKET media MAX n` — the largest file a bucket accepts.
//!
//! # Why the ceiling is a count of bytes rather than `5MB`
//!
//! Because `5MB` is not a thing this language can say. Digits touching a letter
//! are a **duration**, whatever the letter is, so `5MB` lexes as a duration with
//! a unit nothing recognises and is refused before the parser sees it. That rule
//! belongs to every literal in the grammar, and changing it to give one clause a
//! shorter spelling would be a lexical change everywhere bought for a
//! convenience here.
//!
//! # The test that matters is the ranged one
//!
//! A whole-file write is over the ceiling or it is not, and a check anywhere on
//! that path catches it. A **ranged** write is the case a plausible
//! implementation gets wrong: it splices into bytes already stored, so a file
//! grows past the ceiling while no single write is anywhere near it. A limit
//! checked against the bytes the statement carried would pass every time and
//! would hold only against callers who were not going to exceed it anyway.
//!
//! So the refusal is checked against the file **as it will be**, at the one
//! point both write shapes have finished computing that — and the ranged test
//! below is what proves the placement rather than the comparison.
//!
//! # What is deliberately not here
//!
//! There is no `HOLDS` clause and no test for one. A bucket's metadata record
//! holds `size`, `chunks` and `updated`, and the store has no content type for a
//! file anywhere — so a clause refusing an upload by type would be enforcing the
//! caller's own claim about the caller's own bytes, which is the lie ADR-0011
//! designed this kind to prevent by refusing `CREATE`, `UPDATE` and `SET` on it.
//! Recorded as Q-330; the honest version of the clause sniffs the bytes.

#![allow(clippy::panic, clippy::unwrap_used, clippy::indexing_slicing)]

use std::sync::Arc;

use tessari_kv::{KvBackend, MemoryBackend};
use tessari_session::{Outcome, Session};
use tessari_storage::Store;
use tessari_types::Value;

fn store() -> Store {
    Store::open(Arc::new(MemoryBackend::new()) as Arc<dyn KvBackend>).unwrap()
}

const TENANCY: &str = "\
DEFINE NAMESPACE prod; USE NAMESPACE prod;
DEFINE DATABASE work; USE DATABASE work;
";

/// A session holding a bucket declared with `declaration` after the name.
fn holding<'a>(store: &'a Store, declaration: &str) -> Session<'a> {
    let mut session = Session::new(store);
    session
        .run(&format!("{TENANCY}DEFINE BUCKET media{declaration};"))
        .unwrap();
    session
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// `n` bytes whose value follows the offset, so a lost splice shows as a wrong
/// value and not only as a wrong length.
fn seeded(size: usize) -> Vec<u8> {
    (0..size)
        .map(|at| u8::try_from(at % 251).unwrap_or(0))
        .collect()
}

/// What a statement said when it was refused, or `None` when it was accepted.
fn refusal(session: &mut Session<'_>, statement: &str) -> Option<String> {
    session.run(statement).err().map(|error| error.to_string())
}

/// The bytes stored at `/big.bin`, or `None` when there are none.
fn stored(session: &mut Session<'_>) -> Option<Vec<u8>> {
    match session
        .run("READ media:'/big.bin';")
        .unwrap()
        .pop()
        .unwrap()
    {
        Outcome::Value(Value::Bytes(held)) => Some(held),
        Outcome::Value(Value::None) => None,
        other => panic!("a read answered with {other:?}"),
    }
}

#[test]
fn a_file_inside_the_ceiling_is_written_and_one_above_it_is_refused() {
    let held = store();
    let mut session = holding(&held, " MAX 100");

    let inside = seeded(100);
    session
        .run(&format!("PUT media:'/big.bin' = 0x{};", hex(&inside)))
        .unwrap();
    assert_eq!(stored(&mut session), Some(inside));

    // One byte over, so the boundary itself is the thing under test: `MAX 100`
    // takes a file of a hundred bytes and refuses a file of a hundred and one.
    let over = seeded(101);
    let said = refusal(
        &mut session,
        &format!("PUT media:'/big.bin' = 0x{};", hex(&over)),
    )
    .expect("the bucket accepted a file above its ceiling");
    assert!(said.contains("101"), "{said}");
    assert!(said.contains("100"), "{said}");
}

#[test]
fn a_ranged_write_that_grows_a_file_past_the_ceiling_is_refused_though_no_single_write_is_near_it()
{
    // The silent bypass, and the reason the check sits where it does. Ninety
    // bytes then twenty more, against a ceiling of a hundred: neither statement
    // carries anything like a hundred bytes, and the file they would leave
    // behind is a hundred and ten.
    let held = store();
    let mut session = holding(&held, " MAX 100");

    let first = seeded(90);
    session
        .run(&format!("PUT media:'/big.bin' = 0x{};", hex(&first)))
        .unwrap();

    let more = seeded(20);
    let said = refusal(
        &mut session,
        &format!("PUT media:'/big.bin' START 90 = 0x{};", hex(&more)),
    )
    .expect("a splice grew the file past the ceiling");
    assert!(said.contains("110"), "{said}");

    // And the refusal left the file alone. A partially applied refusal would be
    // worse than an accepted write, because the store would then hold a file
    // nobody asked for and no statement reports the difference.
    assert_eq!(stored(&mut session), Some(first));
}

#[test]
fn a_ranged_write_that_stays_inside_the_ceiling_is_written() {
    // The other half of the pair above: the check must not refuse a splice
    // merely for being a splice. Ninety bytes overwritten in the middle end as
    // ninety bytes, which is inside.
    let held = store();
    let mut session = holding(&held, " MAX 100");

    let first = seeded(90);
    session
        .run(&format!("PUT media:'/big.bin' = 0x{};", hex(&first)))
        .unwrap();
    session
        .run("PUT media:'/big.bin' START 10 = 0xffff;")
        .unwrap();

    let found = stored(&mut session).expect("the splice was refused");
    assert_eq!(found.len(), 90);
    assert_eq!(found[10..12], [0xff, 0xff]);
    assert_eq!(found[12..], first[12..]);
}

#[test]
fn a_bucket_declared_without_the_clause_takes_whatever_it_is_given() {
    // The absence path, asserted rather than assumed. Every bucket in every
    // store predates this clause, and a ceiling that defaulted to some number
    // would refuse files those stores accept today — so the test is not that
    // the clause is optional in the grammar but that leaving it out changes
    // nothing about what the bucket does.
    let held = store();
    let mut session = holding(&held, "");

    let big = seeded(4096);
    session
        .run(&format!("PUT media:'/big.bin' = 0x{};", hex(&big)))
        .unwrap();
    assert_eq!(stored(&mut session), Some(big));
}

#[test]
fn the_ceiling_reads_back_in_the_statement_that_would_recreate_the_bucket() {
    // Without this the script re-executes happily and the bucket comes back
    // unbounded — a restored store that accepts a file the original refused,
    // with nothing anywhere in an error state to say so.
    let held = store();
    let mut session = holding(&held, " MAX 5242880");

    let report = match session.run("INFO FOR TABLE media;").unwrap().pop().unwrap() {
        Outcome::Value(value) => value,
        other => panic!("{other:?}"),
    };
    let Value::Object(fields) = &report else {
        panic!("{report:?}");
    };
    let Some(Value::String(definition)) = fields.get("definition") else {
        panic!("no definition in {fields:?}");
    };
    assert!(
        definition.contains("DEFINE BUCKET media MAX 5242880"),
        "{definition}"
    );
}

#[test]
fn a_bucket_with_no_ceiling_reads_back_without_the_clause() {
    // The other direction of the round trip: a script carrying `MAX` for a
    // bucket that declared none would re-create a narrower store than the one
    // it describes.
    let held = store();
    let mut session = holding(&held, "");

    let report = match session.run("INFO FOR TABLE media;").unwrap().pop().unwrap() {
        Outcome::Value(value) => value,
        other => panic!("{other:?}"),
    };
    let Value::Object(fields) = &report else {
        panic!("{report:?}");
    };
    let Some(Value::String(definition)) = fields.get("definition") else {
        panic!("no definition in {fields:?}");
    };
    assert!(definition.contains("DEFINE BUCKET media"), "{definition}");
    assert!(!definition.contains("MAX"), "{definition}");
}

#[test]
fn a_ceiling_of_nothing_is_refused_at_the_declaration() {
    // `MAX 0` is a bucket that accepts no file at all, which is a table nothing
    // can be written to rather than a bucket with a limit — so it refuses where
    // it is written instead of being stored as a store nobody can use.
    let held = store();
    let mut session = Session::new(&held);
    session.run(TENANCY).unwrap();
    assert!(
        refusal(&mut session, "DEFINE BUCKET media MAX 0;").is_some(),
        "a bucket that accepts nothing was declared"
    );
    assert!(
        refusal(&mut session, "DEFINE BUCKET media MAX -1;").is_some(),
        "a negative ceiling was declared"
    );
}
