//! `rand::uuid()` — a fresh identifier per record, and what that rests on.
//!
//! # The mirror of `instants.rs`, and the reason both are needed
//!
//! One mechanism decides both answers. `plan::fold` evaluates the parts of an
//! expression that do not depend on a record **once above the records**, and for
//! the twenty-nine functions the language had before this one, "reads no record"
//! and "is safe to evaluate once" were the same property. `instants.rs` pins the
//! side of that where folding is the guarantee: one statement observes one
//! instant precisely *because* `time::now()` is folded.
//!
//! This file pins the side where folding would be the defect. `rand::uuid()`
//! reads no record either, and folded like a constant it writes **one**
//! identifier into every row of a read — no error, no failing test, and nothing
//! visible until two records that should differ do not. So the fold now asks two
//! questions instead of one, and consults `Function::purity` for the second.
//!
//! # Why these two files must both keep passing
//!
//! The cheap way to make this file pass is to stop folding, and that silently
//! breaks the other one. The cheap way to make that file pass is to fold
//! everything, which is what produced the defect this one describes. Neither
//! guarantee is safe on its own, which is why the assertion that matters most
//! here — `the_clock_still_folds_while_the_generator_does_not` — asks for both
//! in a single statement.

#![allow(clippy::panic, clippy::unwrap_used)]

use std::collections::BTreeSet;
use std::sync::Arc;

use tessari_kv::{KvBackend, MemoryBackend};
use tessari_session::{Outcome, Session};
use tessari_storage::Store;
use tessari_types::Value;

/// Enough records that a folded identifier could not pass by luck.
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

fn distinct(values: &[Value]) -> usize {
    values.iter().cloned().collect::<BTreeSet<Value>>().len()
}

/// The defect Q-221 named, asserted from the outside.
#[test]
fn every_record_of_one_read_gets_an_identifier_of_its_own() {
    let store = store();
    let mut session = ready(&store);
    let answered = run(&mut session, "SELECT rand::uuid() AS id FROM ticks;");
    let ids = column(&answered, "id");

    assert_eq!(ids.len(), MANY, "the read did not see every record");
    assert!(
        matches!(ids[0], Value::Uuid(_)),
        "the identifier is not a uuid: {:?}",
        ids[0]
    );
    let seen = distinct(&ids);
    assert_eq!(
        seen, MANY,
        "{MANY} records were handed {seen} distinct identifiers — the generator \
         was evaluated above the records and every row took the same one"
    );
}

/// The assertion neither guarantee survives without.
///
/// Folding is the mechanism behind both answers, so a change that satisfies one
/// of them by turning the fold off or on wholesale fails here: the two columns
/// come out of one statement, one read, and one pass of the same optimiser.
#[test]
fn the_clock_still_folds_while_the_generator_does_not() {
    let store = store();
    let mut session = ready(&store);
    let answered = run(
        &mut session,
        "SELECT time::now() AS at, rand::uuid() AS id FROM ticks;",
    );

    let instants = column(&answered, "at");
    assert_eq!(
        distinct(&instants),
        1,
        "the clock reached the records unfolded — this statement observed more \
         than one moment"
    );

    let ids = column(&answered, "id");
    assert_eq!(
        distinct(&ids),
        MANY,
        "the generator was folded alongside the clock, so every row took one id"
    );
}

/// The walk asks the whole tree, not the outermost node.
///
/// `type::string(rand::uuid())` reads no record at either level, so a check that
/// only looked at the call it was folding would fold this one and write a single
/// piece of text into every row.
#[test]
fn a_generator_nested_inside_another_call_is_still_asked_per_record() {
    let store = store();
    let mut session = ready(&store);
    let answered = run(
        &mut session,
        "SELECT type::string(rand::uuid()) AS id FROM ticks;",
    );
    let ids = column(&answered, "id");

    assert!(
        matches!(ids[0], Value::String(_)),
        "the cast did not produce text: {:?}",
        ids[0]
    );
    assert_eq!(
        distinct(&ids),
        MANY,
        "a generator one level down was folded with the call around it"
    );
}

/// The order's keys are folded by a second path, and it needed the same fix.
///
/// `folded_order` is a separate call site from the projection's, so the two can
/// be corrected apart. Ordering by a generated identifier is a shuffle; folded,
/// it is one constant key and the records come back in the order they were
/// stored, which is a read that looks like it worked.
#[test]
fn ordering_by_a_generated_identifier_shuffles_rather_than_ordering_by_nothing() {
    let store = store();
    let mut session = ready(&store);
    let answered = run(&mut session, "SELECT n FROM ticks ORDER BY rand::uuid();");
    let order = column(&answered, "n");

    assert_eq!(order.len(), MANY);
    let stored: Vec<Value> = (1..=MANY)
        .map(|at| Value::from(i64::try_from(at).unwrap()))
        .collect();
    assert_ne!(
        order, stored,
        "the records came back in the order they were stored, so the sort key \
         was one folded constant"
    );
    // And nothing was lost or duplicated on the way through the sort.
    assert_eq!(distinct(&order), MANY);
}

/// The case a person actually writes, which was already safe.
///
/// A field's `DEFAULT` is parsed and evaluated per write, from the catalog, and
/// never reaches `plan::fold` at all — so this passed before the fix and passes
/// after. It is pinned because it is the shape the language is *for*, and
/// because a later change that routed defaults through the planner would break
/// it in exactly the way the projection was broken.
#[test]
fn a_default_of_a_generated_identifier_gives_each_record_its_own() {
    let store = store();
    let mut session = ready(&store);
    session
        .run(
            "DEFINE TABLE events; \
             DEFINE FIELD id ON events TYPE uuid DEFAULT rand::uuid();",
        )
        .unwrap();
    for at in 1..=3 {
        session
            .run(&format!("CREATE events:{at} = {{ what: 'opened' }};"))
            .unwrap();
    }
    let answered = run(&mut session, "SELECT id FROM events;");
    let ids = column(&answered, "id");

    assert_eq!(ids.len(), 3);
    assert_eq!(
        distinct(&ids),
        3,
        "three records share an identifier: {ids:?}"
    );
}
