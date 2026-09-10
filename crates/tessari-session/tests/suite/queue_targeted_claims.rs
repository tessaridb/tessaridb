//! `CLAIM jobs:7` — the hold on a record the caller names.
//!
//! The selecting form chooses; this one is told. Everything else about a hold is
//! the same value written to the same two fields, which is the whole design: a
//! second door into one write rather than a second mechanism.
//!
//! Four properties this module exists to pin, and each is one a plausible
//! implementation gets wrong:
//!
//! - **it takes what it was told to take**, so the walk afterwards passes over
//!   it exactly as it would over a hold the walk itself took;
//! - **contention is an answer and absence is an error** — a record somebody
//!   holds answers nothing without raising, while a record that is not there
//!   raises, because a caller who named a record has to be told which of the two
//!   happened;
//! - **it observes its own transaction's hold**, which the selecting form
//!   deliberately does not (its writes land after its walk so a claim cannot
//!   count one record twice); the two forms reach the record by different paths
//!   and this asserts they agree about what a transaction sees of itself;
//! - **it reports a record lookup**, not the scan the selecting form reports —
//!   the natural implementation is a copy of that form and inherits its plan
//!   line, which would be a statement lying about how it reached the record.
//!
//! The last two are the critique's conditions 2 and 3
//! (`critiques/20260910-1310-targeted-claim-design/`). They are here because a
//! condition that lives in a report is a condition nobody runs.

#![allow(clippy::panic, clippy::unwrap_used)]

use std::sync::Arc;

use tessari_kv::{KvBackend, MemoryBackend};
use tessari_session::{AccessPath, Outcome, Session};
use tessari_storage::Store;

fn store() -> Store {
    Store::open(Arc::new(MemoryBackend::new()) as Arc<dyn KvBackend>).unwrap()
}

/// A queue holding three pieces of work, under the declaration the test names.
fn ready<'a>(store: &'a Store, declaration: &str) -> Session<'a> {
    let mut session = Session::new(store);
    session
        .run(&format!(
            "DEFINE NAMESPACE prod; USE NAMESPACE prod;\n\
             DEFINE DATABASE shop; USE DATABASE shop;\n\
             DEFINE QUEUE jobs {declaration};\n\
             CREATE jobs:1 = {{ url: 'a' }};\n\
             CREATE jobs:2 = {{ url: 'b' }};\n\
             CREATE jobs:3 = {{ url: 'c' }};"
        ))
        .unwrap();
    session
}

/// The identities one statement answered with, in the order it answered.
fn claimed(outcome: &Outcome) -> Vec<String> {
    match outcome {
        Outcome::Records { records, .. } => records.iter().map(|(id, _)| id.to_string()).collect(),
        other => panic!("expected records, got {other:?}"),
    }
}

/// How a statement says it reached its records.
fn access(outcome: &Outcome) -> AccessPath {
    match outcome {
        Outcome::Records { plan, .. } => plan.access,
        other => panic!("expected records, got {other:?}"),
    }
}

/// One statement's outcome.
fn run(session: &mut Session<'_>, script: &str) -> Outcome {
    session.run(script).unwrap().pop().unwrap()
}

#[test]
fn a_record_claimed_by_name_is_held_and_the_walk_passes_over_it() {
    let store = store();
    let mut session = ready(&store, "TIMEOUT 30s");

    let named = run(&mut session, "CLAIM jobs:2;");
    let walked = run(&mut session, "CLAIM 3 FROM jobs;");

    assert_eq!(
        claimed(&named),
        vec!["2"],
        "the record named is the record taken"
    );
    assert_eq!(
        claimed(&walked),
        vec!["1", "3"],
        "the walk passes over the named hold exactly as it would over its own"
    );
}

#[test]
fn a_record_another_claim_holds_answers_nothing_and_no_error() {
    let store = store();
    let mut session = ready(&store, "TIMEOUT 30s");

    run(&mut session, "CLAIM jobs:2;");
    let again = run(&mut session, "CLAIM jobs:2;");

    // The whole of the "contention is an answer" decision. If this is ever
    // reversed into a refusal, a worker polling for a record somebody else has
    // starts seeing failures on a healthy queue.
    assert!(
        claimed(&again).is_empty(),
        "a held record answers nothing rather than raising"
    );
}

#[test]
fn a_record_that_does_not_exist_is_refused_naming_it() {
    let store = store();
    let mut session = ready(&store, "TIMEOUT 30s");

    let refusal = session.run("CLAIM jobs:99;").unwrap_err().to_string();

    assert!(
        refusal.contains("record") && refusal.contains("99"),
        "absence is an error and it names what was missing, got {refusal}"
    );
}

#[test]
fn release_clears_a_targeted_hold_and_the_record_is_claimable_again() {
    let store = store();
    let mut session = ready(&store, "TIMEOUT 30s");

    run(&mut session, "CLAIM jobs:2;");
    run(&mut session, "RELEASE jobs:2;");
    let again = run(&mut session, "CLAIM jobs:2;");

    assert_eq!(
        claimed(&again),
        vec!["2"],
        "the pair a worker writes is a pair: what CLAIM takes, RELEASE gives back"
    );
}

#[test]
fn each_targeted_claim_counts_an_attempt_and_the_ceiling_stops_it() {
    let store = store();
    let mut session = ready(&store, "TIMEOUT 1ns ATTEMPTS 2");

    // A one-nanosecond hold has lapsed by the next read, so the ceiling is the
    // only thing left that can stop the third claim — which is what this
    // asserts. The same trick the selecting form's own expiry cases use.
    let first = run(&mut session, "CLAIM jobs:1;");
    let second = run(&mut session, "CLAIM jobs:1;");
    let third = run(&mut session, "CLAIM jobs:1;");

    assert_eq!(claimed(&first), vec!["1"]);
    assert_eq!(claimed(&second), vec!["1"], "a lapsed hold is retaken");
    assert!(
        claimed(&third).is_empty(),
        "the attempt count is taken at the hand-out however the record was chosen"
    );
}

#[test]
fn a_targeted_claim_on_a_plain_table_is_refused_as_not_a_queue() {
    let store = store();
    let mut session = Session::new(&store);
    session
        .run(
            "DEFINE NAMESPACE prod; USE NAMESPACE prod;\n\
             DEFINE DATABASE shop; USE DATABASE shop;\n\
             DEFINE COLLECTION notes;\n\
             CREATE notes:1 = { text: 'a' };",
        )
        .unwrap();

    let refusal = session.run("CLAIM notes:1;").unwrap_err().to_string();

    assert!(
        refusal.contains("notes"),
        "the targeted form is not a back door into a plain table, got {refusal}"
    );
}

#[test]
fn a_second_targeted_claim_in_one_transaction_sees_the_hold_the_first_wrote() {
    let store = store();
    let mut session = ready(&store, "TIMEOUT 30s");

    let outcomes = session
        .run("BEGIN; CLAIM jobs:1; CLAIM jobs:1; COMMIT;")
        .unwrap();

    // Index 0 is the BEGIN, 1 and 2 the two claims, 3 the COMMIT.
    assert_eq!(claimed(&outcomes[1]), vec!["1"], "the first claim takes it");
    assert!(
        claimed(&outcomes[2]).is_empty(),
        "the second sees the hold the first wrote — the two forms agree about \
         what a transaction observes of itself"
    );
}

#[test]
fn the_targeted_claim_reports_a_record_lookup_rather_than_a_scan() {
    let store = store();
    let mut session = ready(&store, "TIMEOUT 30s");

    let named = run(&mut session, "CLAIM jobs:2;");
    let walked = run(&mut session, "CLAIM FROM jobs;");

    assert_eq!(
        access(&named),
        AccessPath::Record,
        "a claim that was told which record did not walk the table"
    );
    assert_eq!(
        access(&walked),
        AccessPath::Scan,
        "and the selecting form still reports the walk it performs"
    );
}

#[test]
fn a_claim_naming_a_table_without_an_identity_names_both_forms() {
    let store = store();
    let mut session = ready(&store, "TIMEOUT 30s");

    let refusal = session.run("CLAIM jobs;").unwrap_err().to_string();

    // The likely mistake is a missing `FROM`, not a missing identity, so the
    // message says what a claim can look like rather than only what the parser
    // wanted next.
    assert!(
        refusal.contains("FROM") && refusal.contains("jobs:"),
        "the refusal names both forms, got {refusal}"
    );
}
