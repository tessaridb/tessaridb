//! Row claims a locking read takes, and the things the design promises.
//!
//! The claims design (the round-four study, §2) maps the dialect's
//! `FOR UPDATE [NOWAIT | SKIP LOCKED]` onto this engine's discipline:
//! snapshot isolation, no lock manager, contention reported rather than
//! waited. A locking read takes a per-record claim in the store's claim
//! register — the same leader-local register a queue claim takes, which is
//! the point: one register, one rule that a claim releases when the
//! transaction ends, whichever way it ends.
//!
//! The properties asserted here are the ones the design states, in its own
//! words:
//!
//! - a claimed row refuses another transaction's write with
//!   `ClaimedByAnother`, class `conflict` — the retry is the caller's;
//! - `SKIP LOCKED` omits claimed rows at read time; `NOWAIT` (the default)
//!   refuses, naming the wait the engine will not do;
//! - claims release at commit and at cancel, and die with the connection —
//!   the transaction's `Drop` gives back what it was claiming;
//! - rolling back across a claim refuses (`SavepointCrossesClaim` extends
//!   to row claims — the register sits outside the savepoint buffer);
//! - the locking clause is refused by name beside aggregation and on
//!   anything but a plain single-table read;
//! - a locking read outside a block answers and lets go at statement end.

#![allow(clippy::panic, clippy::unwrap_used, clippy::indexing_slicing)]

use std::sync::Arc;
use tessari_session::Now;
use tessari_session::RunNow;

use tessari_kv::{KvBackend, MemoryBackend};
use tessari_session::{Outcome, Session};
use tessari_storage::Store;
use tessari_types::Value;

fn store() -> Store {
    Store::open(Arc::new(MemoryBackend::new()) as Arc<dyn KvBackend>)
        .now()
        .unwrap()
}

/// Three rows in `prod.shop`, ready for two sessions to race over.
fn ready(store: &Store) -> Session<'_> {
    let mut session = Session::new(store);
    session
        .run_now(
            "DEFINE NAMESPACE prod; USE NAMESPACE prod;\n\
             DEFINE DATABASE shop; USE DATABASE shop;\n\
             DEFINE COLLECTION jobs;\n\
             CREATE jobs:1 = { url: 'a' };\n\
             CREATE jobs:2 = { url: 'b' };\n\
             CREATE jobs:3 = { url: 'c' };",
        )
        .unwrap();
    session
}

/// A second session in the tenancy `ready` declares.
fn worker(store: &Store) -> Session<'_> {
    let mut session = Session::new(store);
    session
        .run_now("USE NAMESPACE prod; USE DATABASE shop;")
        .unwrap();
    session
}

/// The identities one statement answered with, in answer order.
fn claimed(outcome: &Outcome) -> Vec<String> {
    match outcome {
        Outcome::Records { records, .. } => records.iter().map(|(id, _)| id.to_string()).collect(),
        other => panic!("expected records, got {other:?}"),
    }
}

/// One statement's outcome.
fn run(session: &mut Session<'_>, script: &str) -> Outcome {
    session.run_now(script).unwrap().pop().unwrap()
}

/// The identities a statement on an ordinary session answered with.
fn run_claimed(session: &mut Session<'_>, script: &str) -> Vec<String> {
    claimed(&run(session, script))
}

/// The identities a statement inside an open block answered with.
async fn claimed_in(open: &mut tessari_session::Atomic<'_, '_>, script: &str) -> Vec<String> {
    claimed(
        &open
            .run_with(script, &tessari_session::Parameters::new())
            .await
            .unwrap()
            .pop()
            .unwrap(),
    )
}

#[test]
fn a_locking_read_claims_and_another_write_refuses_until_the_commit() {
    let store = store();
    let mut first = ready(&store);
    let mut second = worker(&store);
    first
        .atomically(async |open| -> Result<(), tessari_session::Error> {
            assert_eq!(
                claimed_in(open, "SELECT * FROM jobs:1 FOR UPDATE;").await,
                ["1"]
            );
            // The claim holds: a write from another transaction is refused,
            // named, and the refusal is the conflict class — retried, not
            // blindly.
            let refused = second
                .run("UPDATE jobs:1 SET url = 'z';")
                .await
                .unwrap_err();
            assert!(
                matches!(refused, tessari_session::Error::ClaimedByAnother { .. }),
                "the write was not refused with ClaimedByAnother: {refused:?}"
            );
            Ok(())
        })
        .now()
        .unwrap();
    // Committed: the claim is back with the register, the write applies.
    second.run_now("UPDATE jobs:1 SET url = 'z';").unwrap();
    assert_eq!(run_claimed(&mut second, "SELECT url FROM jobs:1;").len(), 1);
}

#[test]
fn skip_locked_omits_a_claimed_row_and_answers_the_rest() {
    let store = store();
    let mut first = ready(&store);
    let mut second = worker(&store);
    first
        .atomically(async |open| -> Result<(), tessari_session::Error> {
            claimed_in(open, "SELECT * FROM jobs:1 FOR UPDATE;").await;
            // The second session sees neither the claim nor the claimed row:
            // the claim is register state, and the row is passed over.
            let answered = second
                .run("SELECT id FROM jobs FOR UPDATE SKIP LOCKED;")
                .await
                .unwrap()
                .pop()
                .unwrap();
            assert_eq!(
                claimed(&answered),
                ["2", "3"],
                "SKIP LOCKED did not omit the claimed row"
            );
            Ok(())
        })
        .now()
        .unwrap();
}

#[test]
fn nowait_refuses_when_a_row_it_touched_is_claimed_and_names_the_wait() {
    let store = store();
    let mut first = ready(&store);
    let mut second = worker(&store);
    first
        .atomically(async |open| -> Result<(), tessari_session::Error> {
            claimed_in(open, "SELECT * FROM jobs:1 FOR UPDATE;").await;
            // NOWAIT is the default: the read refuses rather than waits —
            // the refusal names the wait the engine will not do.
            let refused = second
                .run("SELECT * FROM jobs:1 FOR UPDATE;")
                .await
                .unwrap_err();
            let said = refused.to_string();
            assert!(
                matches!(refused, tessari_session::Error::ClaimedByAnother { .. }),
                "NOWAIT did not refuse: {said}"
            );
            assert!(
                said.contains("never waits") || said.contains("does not wait"),
                "the refusal does not name the wait: {said}"
            );
            Ok(())
        })
        .now()
        .unwrap();
}

#[test]
fn a_cancelled_block_gives_its_claims_back() {
    let store = store();
    let mut first = ready(&store);
    let mut second = worker(&store);
    let cancelled = first
        .atomically(async |open| -> Result<(), tessari_session::Error> {
            claimed_in(open, "SELECT * FROM jobs:1 FOR UPDATE;").await;
            Err(tessari_session::Error::UnclosedTransaction {
                span: tessari_ql::Span::new(0, 0),
            })
        })
        .now();
    assert!(cancelled.is_err());
    // The cancel released the claim with everything else the block held.
    assert_eq!(
        run_claimed(&mut second, "SELECT id FROM jobs FOR UPDATE SKIP LOCKED;"),
        ["1", "2", "3"]
    );
}

#[test]
fn rolling_back_across_a_claim_refuses_and_changes_nothing() {
    let store = store();
    let mut first = ready(&store);
    // The claim is taken AFTER the savepoint, so rolling back to it would
    // have to give a claim back — and the register holds claims outside the
    // savepoint buffer, exactly like a queue claim.
    let refused = first
        .run_now(
            "BEGIN;\n\
             SAVEPOINT held;\n\
             SELECT * FROM jobs:1 FOR UPDATE;\n\
             UPDATE jobs:1 SET url = 'z';\n\
             ROLLBACK TO held;",
        )
        .unwrap_err();
    let said = refused.to_string();
    assert!(
        format!("{refused:?}").contains("SavepointCrossesClaim"),
        "rollback across a row claim was not refused: {said}"
    );
    assert!(
        said.contains("took a claim"),
        "the refusal does not name the claim: {said}"
    );
}

#[test]
fn a_claim_outside_a_block_dies_at_statement_end() {
    let store = store();
    let mut first = ready(&store);
    let mut second = worker(&store);
    // No BEGIN: the statement is its own transaction, and the claim ends
    // where the statement does — the next statement takes it freely.
    assert_eq!(
        run_claimed(&mut first, "SELECT id FROM jobs FOR UPDATE;"),
        ["1", "2", "3"]
    );
    assert_eq!(
        run_claimed(&mut second, "SELECT id FROM jobs FOR UPDATE SKIP LOCKED;"),
        ["1", "2", "3"]
    );
}

#[test]
fn a_row_the_statement_wrote_is_its_own_no_refusal() {
    let store = store();
    let mut first = ready(&store);
    first
        .atomically(async |open| -> Result<(), tessari_session::Error> {
            // Claim it, then write it: the claim is this transaction's own.
            claimed_in(open, "SELECT * FROM jobs:1 FOR UPDATE;").await;
            open.run_with(
                "UPDATE jobs:1 SET url = 'mine';",
                &tessari_session::Parameters::new(),
            )
            .await
            .unwrap();
            Ok(())
        })
        .now()
        .unwrap();
    let Outcome::Records { records, .. } = run(&mut first, "SELECT url FROM jobs:1;") else {
        panic!("expected records");
    };
    let Value::Object(fields) = &records[0].1 else {
        panic!("expected an object");
    };
    assert_eq!(fields.get("url"), Some(&Value::from("mine")));
}

#[test]
fn the_note_reports_how_many_rows_the_read_claimed() {
    let store = store();
    let mut session = ready(&store);
    let outcome = run(&mut session, "SELECT id FROM jobs FOR UPDATE SKIP LOCKED;");
    let Outcome::Records { notes, .. } = outcome else {
        panic!("expected records");
    };
    assert!(
        notes.iter().any(|note| matches!(
            note,
            tessari_session::Note::Claimed { took } if *took == 3
        )),
        "the claimed note with the count is missing: {notes:?}"
    );
}

#[test]
fn locking_beside_aggregation_is_refused_by_name() {
    let store = store();
    let mut session = ready(&store);
    let refused = session
        .run_now("SELECT count(*) AS n FROM jobs FOR UPDATE;")
        .unwrap_err()
        .to_string();
    assert!(
        refused.contains("FOR UPDATE") || refused.contains("locking"),
        "the refusal does not name the clause: {refused}"
    );
}

#[test]
fn locking_on_anything_but_a_plain_single_table_read_is_refused() {
    let store = store();
    let mut session = ready(&store);
    // A combined read cannot name rows to lock — the collection is values.
    let refused = session
        .run_now("SELECT id FROM jobs UNION SELECT id FROM jobs FOR UPDATE;")
        .unwrap_err()
        .to_string();
    assert!(
        refused.contains("FOR UPDATE") || refused.contains("locking"),
        "the refusal does not name the clause: {refused}"
    );
}

#[test]
fn of_names_the_table_it_claims_and_a_stranger_is_refused() {
    let store = store();
    let mut session = ready(&store);
    session
        .run_now("SELECT id FROM jobs WHERE url = 'a' FOR UPDATE OF jobs;")
        .unwrap();
    let refused = session
        .run_now("SELECT id FROM jobs FOR UPDATE OF missing;")
        .unwrap_err()
        .to_string();
    assert!(
        refused.contains("missing"),
        "the refusal does not name the table: {refused}"
    );
}
