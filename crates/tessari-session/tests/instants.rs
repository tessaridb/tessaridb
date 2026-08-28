//! `time::now()` — one instant per statement, and what that rests on.
//!
//! # Why these exist
//!
//! `Function::TimeNow` is documented as *"the instant the statement is evaluated
//! at"*, and the language keeps that promise. Nothing here was broken.
//!
//! What was missing is that **nothing tested it**, and the mechanism holding it
//! up is not where a reader would look. `call` reads the clock afresh every time
//! it runs, and the evaluator dispatches a call straight into `call` — read those
//! two in isolation and the obvious conclusion is that the instant is per
//! *record*. It is not, because `plan::fold` evaluates the record-independent
//! parts of an expression **once above the records**, and `time::now()` reads no
//! record. That module says so in as many words:
//!
//! > It cannot change an answer, with one exception that improves one. […] The
//! > exception is `time::now()`, which was evaluated per record — so one
//! > statement could observe two instants and sort by them. Folded, one statement
//! > observes one instant, which is what a read should mean.
//!
//! # Why that is worth a test rather than a shrug
//!
//! `plan::fold` is introduced, measured and justified as a **performance**
//! optimisation — its doc comment is mostly a benchmark about rebuilding a
//! 32-element literal two thousand times. The semantic guarantee is one sentence
//! at the end of it. A guarantee carried by an optimiser pass, asserted in prose
//! and pinned by no test, is one refactor away from being quietly withdrawn: skip
//! the fold for some position, or add a position the fold does not reach, and
//! `ORDER BY time::now()` becomes an order that regenerates under its own
//! comparator, with nothing to say so.
//!
//! So these do not defend against a bug that exists. They defend that sentence
//! from the next person to make the fold faster.

#![allow(clippy::panic, clippy::unwrap_used)]

use std::sync::Arc;

use tessari_kv::{KvBackend, MemoryBackend};
use tessari_session::{Outcome, Session};
use tessari_storage::Store;
use tessari_types::Value;

/// Enough records that a per-record clock read could not answer alike by luck.
///
/// Measured rather than assumed: with the fold bypassed, two `time::now()` calls
/// written side by side in one statement already differ, so the clock here
/// resolves finely enough that a read touching two hundred records would show
/// it.
const MANY: usize = 200;

fn store() -> Store {
    Store::open(Arc::new(MemoryBackend::new()) as Arc<dyn KvBackend>).unwrap()
}

fn ready(store: &Store) -> Session<'_> {
    let mut session = Session::new(store);
    let mut script = String::from(
        "\
DEFINE NAMESPACE prod; USE NAMESPACE prod;
DEFINE DATABASE shop; USE DATABASE shop;
DEFINE TABLE ticks;
",
    );
    for at in 1..=MANY {
        script.push_str(&format!("CREATE ticks:{at} = {{ n: {at} }};\n"));
    }
    session.run(&script).unwrap();
    session
}

fn run(session: &mut Session<'_>, script: &str) -> Outcome {
    session
        .run(script)
        .unwrap_or_else(|error| panic!("{script}: {error}"))
        .pop()
        .expect("one outcome")
}

fn field<'a>(record: &'a Value, name: &str) -> Option<&'a Value> {
    let Value::Object(fields) = record else {
        panic!("not an object")
    };
    fields.get(name)
}

/// The value every record answered under one name.
fn column(answered: &Outcome, name: &str) -> Vec<Value> {
    answered
        .records()
        .expect("records")
        .iter()
        .map(|(_, record)| field(record, name).expect("the projected field").clone())
        .collect()
}

/// Every record of one read is asked the same question and gives one answer.
#[test]
fn one_statement_observes_one_instant_however_many_records_it_touches() {
    let store = store();
    let mut session = ready(&store);
    let answered = run(&mut session, "SELECT time::now() AS at FROM ticks;");
    let instants = column(&answered, "at");

    assert_eq!(instants.len(), MANY, "the read did not see every record");
    let first = &instants[0];
    assert!(
        matches!(first, Value::Datetime(_)),
        "the instant is not a datetime: {first:?}"
    );
    let differing = instants.iter().filter(|held| *held != first).count();
    assert_eq!(
        differing, 0,
        "{differing} of {MANY} records saw a different instant — the clock \
         reached the records unfolded, so this statement observed more than one \
         moment"
    );
}

/// The fold is per **expression**, and this says where the guarantee stops.
///
/// Two calls written side by side are two expressions and may answer with two
/// instants. Pinned because it is the boundary of the claim above: what the
/// language promises is that *one* `time::now()` answers alike for every record
/// it is applied to — not that a statement has a single clock reading.
#[test]
fn the_guarantee_is_per_expression_and_this_says_so() {
    let store = store();
    let mut session = ready(&store);
    let answered = run(
        &mut session,
        "SELECT time::now() AS a, time::now() AS b FROM ticks;",
    );
    let (first, second) = (column(&answered, "a"), column(&answered, "b"));
    assert!(
        first.iter().all(|held| *held == first[0]),
        "the first instant varied across records"
    );
    assert!(
        second.iter().all(|held| *held == second[0]),
        "the second instant varied across records"
    );
}

/// The consequence that is not subtle, pinned as behaviour rather than prose.
///
/// A sort key regenerated under the comparator has no order at all. Folded, the
/// key is one value, so the read answers in the order its tiebreaker gives.
#[test]
fn ordering_by_the_instant_is_an_order_and_not_a_shuffle() {
    let store = store();
    let mut session = ready(&store);
    let once = run(&mut session, "SELECT n FROM ticks ORDER BY time::now(), n;");
    let again = run(&mut session, "SELECT n FROM ticks ORDER BY time::now(), n;");
    let ordered: Vec<Value> = column(&once, "n");
    assert_eq!(
        ordered,
        column(&again, "n"),
        "two identical reads answered in different orders"
    );
    assert_eq!(
        ordered.len(),
        MANY,
        "the ordered read lost records on the way"
    );
}

/// Two statements are two observations, and the later one is not earlier.
///
/// Deliberately `>=` rather than `>`: the guarantee is that a statement sees one
/// instant, not that the clock separates two statements. Asserting `>` would be
/// asserting something about the machine.
#[test]
fn a_later_statement_does_not_see_an_earlier_instant() {
    let store = store();
    let mut session = ready(&store);
    let before = run(&mut session, "SELECT time::now() AS at FROM ticks LIMIT 1;");
    let after = run(&mut session, "SELECT time::now() AS at FROM ticks LIMIT 1;");
    let (Value::Datetime(before), Value::Datetime(after)) =
        (&column(&before, "at")[0], &column(&after, "at")[0])
    else {
        panic!("not datetimes")
    };
    assert!(
        after >= before,
        "the second statement saw an earlier instant than the first"
    );
}

/// The instant still composes with the functions that take one.
///
/// `time::bucket` is pure; its argument is not. Folding the argument must not
/// change what can be built on it.
#[test]
fn the_instant_still_composes_with_the_functions_that_take_one() {
    let store = store();
    let mut session = ready(&store);
    let answered = run(
        &mut session,
        "SELECT time::bucket(time::now(), 1h) AS window FROM ticks;",
    );
    let windows = column(&answered, "window");
    assert_eq!(windows.len(), MANY);
    let first = &windows[0];
    assert!(
        matches!(first, Value::Datetime(_)),
        "the window is not a datetime: {first:?}"
    );
    assert_eq!(
        windows.iter().filter(|held| *held != first).count(),
        0,
        "one statement produced more than one window"
    );
}

/// Impurity is **wanted** in a default, and nothing here may refuse it.
///
/// `DEFAULT time::now()` is a created-at column — the reason the category is
/// `Purity::PerStatement` rather than something forbidden.
#[test]
fn a_default_may_still_be_the_instant() {
    let store = store();
    let mut session = ready(&store);
    session
        .run(
            "DEFINE TABLE events; \
             DEFINE FIELD seen ON events TYPE datetime DEFAULT time::now();",
        )
        .unwrap();
    session
        .run("CREATE events:1 = { what: 'opened' };")
        .unwrap();
    let answered = run(&mut session, "SELECT seen FROM events;");
    let seen = column(&answered, "seen");
    assert!(
        matches!(seen.as_slice(), [Value::Datetime(_)]),
        "a default of time::now() did not write an instant: {seen:?}"
    );
}
