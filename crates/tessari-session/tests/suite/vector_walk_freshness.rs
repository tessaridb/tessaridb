//! What the vector walk answers for a record this transaction has just written.
//!
//! # The guard, and which half of Q-500 survived reading
//!
//! Q-500 recorded two refusals the three other bounded walks make about the
//! **store** — as opposed to the caller — and that `Evaluator::walk` appeared to
//! make neither:
//!
//! 1. `Transaction::writes_in` — entries are derived at commit, so a record this
//!    transaction has written has no entry and the graph cannot place it.
//! 2. `!Transaction::indexes_are_current` — entries hold current state and carry
//!    no version, so a reader at an older snapshot is answered from a newer
//!    graph.
//!
//! The second was already true and Q-500 had it wrong. `walk` opens by calling
//! `Context::index_on_path`, which refuses outright when the snapshot is not the
//! committed tail, so a stale reader never reaches the graph — the refusal is
//! made one level up rather than in the walk's own list. That half is stated
//! here as read from the source and is deliberately not asserted by any test in
//! this file: a session cannot hold a transaction open across `run` calls (an
//! unclosed script is `Error::UnclosedTransaction`), so there is no way from
//! this surface to commit underneath an open reader.
//!
//! # What `APPROXIMATE` buys, and what it does not
//!
//! This walk is declared approximate and the caller asked for it by name, so a
//! short answer is inside the contract in a way it is not for the other three
//! walks. That is the reason Q-500 defaulted nothing.
//!
//! The distinction this file rests on: `APPROXIMATE` buys **"the graph missed
//! it"**, which is a property of the structure the caller opted into. It does
//! not buy **"the transaction cannot see its own write"**, which is a statement
//! about isolation that no operator on this statement opts out of. The same
//! defect was fixed for term reads on 2026-09-07, where a record updated *into*
//! a match inside its own transaction was missing from a read that reported
//! itself index-served.
//!
//! # The shape of the case
//!
//! A differential against the exact path in the same transaction, on the same
//! statement, in the same session. A test that only watched the approximate read
//! would have to know the right answer independently; the exact read is that
//! answer, computed by the store, and the two must agree about **membership** of
//! the record the transaction just wrote.
//!
//! The control matters as much: outside a transaction the same read must be
//! served by the graph and must find the same record. Without it, a store that
//! never served the graph at all would pass the case below while saying nothing.

#![allow(clippy::panic, clippy::unwrap_used, clippy::indexing_slicing)]

use std::sync::Arc;

use tessari_kv::{KvBackend, MemoryBackend};
use tessari_session::{AccessPath, Outcome, Session};
use tessari_storage::Store;
use tessari_types::RecordId;

/// How many points the line carries before the transaction adds one.
///
/// Enough that the graph has somewhere to walk and the answer is a small slice
/// of the table rather than most of it.
const POINTS: u32 = 40;

/// The read, from the far end of the line, so the nearest record is the last
/// one written and a newer record placed beyond it becomes the new nearest.
const NEAR: &str = "SELECT * FROM points \
                    ORDER BY vector::euclidean(embedding, [50.0, 0.0]) LIMIT 3";

/// The record the transaction writes: nearer to the query than every committed
/// point, so its absence from an answer is unambiguous rather than a matter of
/// where the graph happened to stop.
const NEARER: &str = "CREATE points:99 = { name: 'written here', embedding: [49.0, 0.0] };";

fn store() -> Store {
    let backend = Arc::new(MemoryBackend::new()) as Arc<dyn KvBackend>;
    Store::open(backend).unwrap()
}

/// Forty points on a line and a vector index over them.
fn ready(store: &Store) -> Session<'_> {
    let mut session = Session::new(store);
    session
        .run(
            "DEFINE NAMESPACE prod; USE NAMESPACE prod;\n\
             DEFINE DATABASE atlas; USE DATABASE atlas;\n\
             DEFINE COLLECTION points;\n\
             DEFINE FIELD embedding ON points TYPE vector<2> REQUIRED;",
        )
        .unwrap();
    let mut script = String::new();
    for n in 0..POINTS {
        script.push_str(&format!(
            "CREATE points:{n} = {{ name: 'point {n}', embedding: [{n}.0, 0.0] }};\n"
        ));
    }
    session.run(&script).unwrap();
    session
        .run("DEFINE INDEX by_embedding ON points FIELDS embedding VECTOR euclidean;")
        .unwrap();
    session
}

/// The records one outcome of a script answered with, and the path that served
/// it.
fn answered(outcomes: &[Outcome], at: usize) -> (Vec<RecordId>, AccessPath) {
    let Some(Outcome::Records { records, plan, .. }) = outcomes.get(at) else {
        panic!("outcome {at} was {:?}", outcomes.get(at));
    };
    (
        records.iter().map(|(id, _)| id.clone()).collect(),
        plan.access,
    )
}

#[test]
fn the_graph_serves_this_read_when_nothing_is_uncommitted() {
    // The control. Every case below is a differential against it, and without it
    // a store that never served the graph would pass them all.
    let held = store();
    let mut session = ready(&held);

    let outcomes = session.run(&format!("{NEAR} APPROXIMATE;")).unwrap();
    let (found, path) = answered(&outcomes, 0);
    assert_eq!(
        path,
        AccessPath::Approximate,
        "the graph did not serve the control: {path:?}"
    );
    assert_eq!(
        found,
        vec![
            RecordId::from(39_i64),
            RecordId::from(38_i64),
            RecordId::from(37_i64)
        ],
        "{found:?}"
    );
}

#[test]
fn a_record_written_in_this_transaction_is_in_its_own_approximate_read() {
    // The case Q-500 is about. The record is created and read inside one
    // transaction, and it is nearer to the query than anything committed, so an
    // answer without it is an answer that cannot see this transaction's own
    // write.
    let held = store();
    let mut session = ready(&held);

    let outcomes = session
        .run(&format!("BEGIN;\n{NEARER}\n{NEAR} APPROXIMATE;\nCOMMIT;"))
        .unwrap();
    let (found, _) = answered(&outcomes, 2);

    assert!(
        found.contains(&RecordId::from(99_i64)),
        "the nearest record was written by this transaction and is missing from \
         its own read: {found:?}"
    );
}

#[test]
fn the_approximate_and_exact_reads_agree_inside_a_writing_transaction() {
    // The differential, and the reason it is not the same case as the one above:
    // that one knows the right answer because the fixture was built to make it
    // obvious, this one does not need to — the exact path computes it, and the
    // two reads must agree about the record the transaction just wrote.
    let held = store();
    let mut session = ready(&held);

    let outcomes = session
        .run(&format!(
            "BEGIN;\n{NEARER}\n{NEAR} APPROXIMATE;\n{NEAR};\nCOMMIT;"
        ))
        .unwrap();
    let (approximate, _) = answered(&outcomes, 2);
    let (exact, _) = answered(&outcomes, 3);

    assert_eq!(
        approximate, exact,
        "the two paths disagree inside a writing transaction: \
         approximate={approximate:?} exact={exact:?}"
    );
}
