//! `FILTER (WHERE …)` and `DISTINCT` on a fold — the S9 combinators — at
//! statement level: known answers worked out from the fixture by hand, the
//! refusals named, and the grammar's edges.

#![allow(clippy::panic, clippy::unwrap_used)]

use std::sync::Arc;
use tessari_session::Now;
use tessari_session::RunNow;

use tessari_kv::{KvBackend, MemoryBackend};
use tessari_session::{Outcome, Session};
use tessari_storage::Store;
use tessari_types::{Number, Value};

fn store() -> Store {
    let backend = Arc::new(MemoryBackend::new()) as Arc<dyn KvBackend>;
    Store::open(backend).now().unwrap()
}

/// Five orders: two paid from ada (10, 10), one paid from bo (7), two open.
fn ready(store: &Store) -> Session<'_> {
    let mut session = Session::new(store);
    session
        .run_now(
            "DEFINE NAMESPACE prod; USE NAMESPACE prod;\n\
             DEFINE DATABASE shop; USE DATABASE shop;\n\
             DEFINE COLLECTION orders;\n\
             CREATE orders = { who: 'ada', amount: 10, status: 'paid' };\n\
             CREATE orders = { who: 'ada', amount: 10, status: 'paid' };\n\
             CREATE orders = { who: 'bo',  amount: 7,  status: 'paid' };\n\
             CREATE orders = { who: 'ada', amount: 5,  status: 'open' };\n\
             CREATE orders = { who: 'bo',  amount: 3,  status: 'open' };",
        )
        .unwrap();
    session
}

/// The one answered value of a one-row read.
fn answered(session: &mut Session<'_>, statement: &str) -> Value {
    let Outcome::Records { records, .. } = session.run_now(statement).unwrap().pop().unwrap()
    else {
        panic!("a read answers with records");
    };
    let (_, row) = records.into_iter().next().unwrap();
    let Value::Object(fields) = row else {
        panic!("a row is an object");
    };
    fields.get("held").cloned().unwrap_or(Value::None)
}

#[test]
fn a_filtered_fold_folds_only_what_the_condition_keeps() {
    let store = store();
    let mut session = ready(&store);
    // 10 + 10 + 7 paid.
    let paid = answered(
        &mut session,
        "SELECT sum(amount) FILTER (WHERE status = 'paid') AS held FROM orders;",
    );
    assert_eq!(paid, Value::Number(Number::Integer(27)), "{paid:?}");
    // count(*) filters too: three paid records.
    let counted = answered(
        &mut session,
        "SELECT count(*) FILTER (WHERE status = 'paid') AS held FROM orders;",
    );
    assert_eq!(counted, Value::Number(Number::Integer(3)), "{counted:?}");
    // A condition nothing meets folds nothing: sum answers its zero, mean
    // its NONE — each fold's own over-nothing rule, unchanged.
    let nothing = answered(
        &mut session,
        "SELECT sum(amount) FILTER (WHERE status = 'refunded') AS held FROM orders;",
    );
    assert_eq!(nothing, Value::Number(Number::Integer(0)), "{nothing:?}");
    let none = answered(
        &mut session,
        "SELECT mean(amount) FILTER (WHERE status = 'refunded') AS held FROM orders;",
    );
    assert_eq!(none, Value::None, "{none:?}");
    // Per group, the filter reads each group's own records.
    let grouped = answered(
        &mut session,
        "SELECT sum(amount) FILTER (WHERE status = 'paid') AS held FROM orders WHERE who = 'bo';",
    );
    assert_eq!(grouped, Value::Number(Number::Integer(7)), "{grouped:?}");
}

#[test]
fn a_distinct_fold_folds_each_value_once() {
    let fixture = store();
    let mut session = ready(&fixture);
    // Distinct amounts: 10, 7, 5, 3 — sum 25 where the plain sum is 35.
    let summed = answered(
        &mut session,
        "SELECT sum(DISTINCT amount) AS held FROM orders;",
    );
    assert_eq!(summed, Value::Number(Number::Integer(25)), "{summed:?}");
    let mean = answered(
        &mut session,
        "SELECT mean(DISTINCT amount) AS held FROM orders;",
    );
    assert_eq!(
        mean,
        Value::Number(Number::Decimal("6.25".parse().unwrap())),
        "{mean:?}"
    );
    // collect keeps first-sighting order.
    let collected = answered(
        &mut session,
        "SELECT collect(DISTINCT amount) AS held FROM orders;",
    );
    assert_eq!(
        collected,
        Value::Array(vec![
            Value::Number(Number::Integer(10)),
            Value::Number(Number::Integer(7)),
            Value::Number(Number::Integer(5)),
            Value::Number(Number::Integer(3)),
        ]),
        "{collected:?}"
    );
    // 3 and 3.0 are one value by the store's own `=`, so a float twin adds
    // nothing to the distinct set.
    let kinds_store = store();
    let mut session = Session::new(&kinds_store);
    session
        .run_now(
            "DEFINE NAMESPACE prod; USE NAMESPACE prod; DEFINE DATABASE shop; \
             USE DATABASE shop; DEFINE COLLECTION kinds; \
             CREATE kinds = { n: 3 }; CREATE kinds = { n: 3.0 }; CREATE kinds = { n: dec 3.0 };",
        )
        .unwrap();
    let one = answered(&mut session, "SELECT sum(DISTINCT n) AS held FROM kinds;");
    assert_eq!(one, Value::Number(Number::Integer(3)), "{one:?}");
}

#[test]
fn the_two_combinators_compose() {
    let store = store();
    let mut session = ready(&store);
    // Paid distinct amounts: 10 and 7 — sum 17.
    let both = answered(
        &mut session,
        "SELECT sum(DISTINCT amount) FILTER (WHERE status = 'paid') AS held FROM orders;",
    );
    assert_eq!(both, Value::Number(Number::Integer(17)), "{both:?}");
}

#[test]
fn what_the_combinators_cannot_mean_is_refused_where_it_is_written() {
    let fixture = store();
    let mut session = ready(&fixture);
    for statement in [
        // A fold that reads an instant refuses DISTINCT rather than guess
        // which argument it was about.
        "SELECT increase(amount, amount) FILTER (WHERE true) AS held FROM orders;",
        "SELECT approx_quantile(DISTINCT amount, 0.5) AS held FROM orders;",
        // FILTER without WHERE is not the grammar.
        "SELECT sum(amount) FILTER (status = 'paid') AS held FROM orders;",
    ] {
        let refused = session.run_now(statement);
        assert!(refused.is_err(), "{statement} ran: {refused:?}");
    }
    // `distinct` alone is still a field being folded, not the word.
    let fields_store = store();
    let mut session = Session::new(&fields_store);
    session
        .run_now(
            "DEFINE NAMESPACE prod; USE NAMESPACE prod; DEFINE DATABASE shop; \
             USE DATABASE shop; DEFINE COLLECTION misc; \
             CREATE misc = { distinct: 4 }; CREATE misc = { distinct: 6 };",
        )
        .unwrap();
    let held = answered(&mut session, "SELECT sum(distinct) AS held FROM misc;");
    assert_eq!(held, Value::Number(Number::Integer(10)), "{held:?}");
}
