//! Composing over a fold.
//!
//! A fold used to be a kind of projection, so `mean(price)` could be asked for
//! and `mean(price) * 1.2` could not be written down at all. It is now an
//! expression node, which is what makes the composition possible — and what
//! makes it need saying that a fold's value is **constant within its group**,
//! because the second pass substitutes it into the tree as a literal and lets
//! the ordinary evaluator finish the job.
//!
//! Every expected number here is worked out by hand from the fixture. A test
//! that computed its expectation from a second fold would be checking that the
//! folding agrees with itself.

#![allow(clippy::panic, clippy::unwrap_used, clippy::indexing_slicing)]

use std::sync::Arc;

use tessari_kv::{KvBackend, MemoryBackend};
use tessari_session::{Error, Session};
use tessari_storage::Store;
use tessari_types::Value;

fn store() -> Store {
    let backend = Arc::new(MemoryBackend::new()) as Arc<dyn KvBackend>;
    Store::open(backend).unwrap()
}

/// Four sales in two cities.
///
/// ```text
/// london: 100, 300   → count 2, sum 400, mean 200
/// paris:   50        → count 1, sum  50, mean  50
/// (a fourth record has no price at all)
/// ```
fn ready(store: &Store) -> Session<'_> {
    let mut session = Session::new(store);
    session
        .run(
            "DEFINE NAMESPACE prod; USE NAMESPACE prod;\n\
             DEFINE DATABASE shop; USE DATABASE shop;\n\
             DEFINE TABLE sales;\n\
             CREATE sales:1 = { city: 'london', price: 100, clerk: 'ada' };\n\
             CREATE sales:2 = { city: 'london', price: 300, clerk: 'grace' };\n\
             CREATE sales:3 = { city: 'paris', price: 50, clerk: 'ada' };\n\
             CREATE sales:4 = { city: 'paris', clerk: 'katherine' };",
        )
        .unwrap();
    session
}

/// The one field of the one answer a read gives, as text.
fn one(session: &mut Session<'_>, script: &str, field: &str) -> String {
    let outcomes = session.run(script).unwrap();
    let records = outcomes[0].records().unwrap();
    assert_eq!(records.len(), 1, "expected one group: {records:?}");
    let Value::Object(fields) = &records[0].1 else {
        panic!("not an object: {:?}", records[0].1);
    };
    format!("{:?}", fields.get(field).unwrap_or(&Value::None))
}

#[test]
fn arithmetic_over_a_fold_answers_the_fold_put_through_the_arithmetic() {
    // The whole feature, in the shape it was asked for. Four sales, three with
    // a price, summing to 450 — so `sum(price) * 2` is 900.
    let store = store();
    let mut session = ready(&store);
    assert_eq!(
        one(
            &mut session,
            "SELECT sum(price) * 2 AS doubled FROM sales;",
            "doubled"
        ),
        one(
            &mut session,
            "SELECT 900 AS doubled FROM sales:1;",
            "doubled"
        )
    );
}

#[test]
fn two_folds_in_one_expression_are_two_folds() {
    // `sum(price) / count(*)` is 450 / 4 — which is deliberately **not**
    // `mean(price)`, because `mean` passes over the record with no price and
    // `count(*)` counts it. Getting 150 here would mean the two occurrences had
    // been collapsed into one.
    let store = store();
    let mut session = ready(&store);
    let answer = one(
        &mut session,
        "SELECT sum(price) / count(*) AS per_sale FROM sales;",
        "per_sale",
    );
    let expected = one(
        &mut session,
        "SELECT 450 / 4 AS per_sale FROM sales:1;",
        "per_sale",
    );
    assert_eq!(answer, expected);
    let mean = one(
        &mut session,
        "SELECT mean(price) AS per_sale FROM sales;",
        "per_sale",
    );
    assert_ne!(
        answer, mean,
        "the two folds were treated as one: this is mean(price), not sum/count(*)"
    );
}

#[test]
fn a_fold_inside_a_call_is_a_value_like_any_other() {
    let store = store();
    let mut session = ready(&store);
    assert_eq!(
        one(
            &mut session,
            "SELECT string::upper(max(clerk)) AS last FROM sales;",
            "last"
        ),
        one(
            &mut session,
            "SELECT 'KATHERINE' AS last FROM sales:1;",
            "last"
        )
    );
}

#[test]
fn a_composed_fold_is_computed_per_group() {
    // London's two sales total 400 and Paris's one totals 50, so doubling gives
    // 800 and 100 — one value per group, not one value for the read.
    let store = store();
    let mut session = ready(&store);
    let outcomes = session
        .run("SELECT city, sum(price) * 2 AS doubled FROM sales GROUP BY city ORDER BY city;")
        .unwrap();
    let records = outcomes[0].records().unwrap();
    assert_eq!(records.len(), 2);
    let held: Vec<String> = records
        .iter()
        .map(|(_, value)| {
            let Value::Object(fields) = value else {
                panic!("not an object");
            };
            format!(
                "{:?}/{:?}",
                fields.get("city").unwrap_or(&Value::None),
                fields.get("doubled").unwrap_or(&Value::None)
            )
        })
        .collect();
    assert_eq!(
        held[0], r#"String("london")/Number(Integer(800))"#,
        "{held:?}"
    );
    assert_eq!(
        held[1], r#"String("paris")/Number(Integer(100))"#,
        "{held:?}"
    );
}

#[test]
fn a_bare_fold_still_answers_exactly_as_it_did() {
    // The refactor's floor. Everything else in the suite says this too, and it
    // is worth one direct statement in the file that changed the shape.
    let store = store();
    let mut session = ready(&store);
    assert_eq!(
        one(&mut session, "SELECT count(*) AS n FROM sales;", "n"),
        one(&mut session, "SELECT 4 AS n FROM sales:1;", "n")
    );
    assert_eq!(
        one(
            &mut session,
            "SELECT sum(price) AS total FROM sales;",
            "total"
        ),
        one(&mut session, "SELECT 450 AS total FROM sales:1;", "total")
    );
}

#[test]
fn a_fold_folding_over_a_fold_is_refused() {
    // The inner fold has already collapsed the records the outer one would fold
    // over, so what is left to average is one number.
    let store = store();
    let mut session = ready(&store);
    let refused = session.run("SELECT mean(sum(price)) AS nonsense FROM sales;");
    assert!(
        matches!(refused, Err(Error::Script(_))),
        "a nested fold was accepted: {refused:?}"
    );
}

#[test]
fn a_fold_in_a_filter_is_refused_and_says_what_it_would_be() {
    // A filter sees one record at a time. What this asks for is a filter over
    // *groups*, which is `HAVING` — and the refusal has to say so, because
    // "unexpected token" would send the author looking for a typo.
    let store = store();
    let mut session = ready(&store);
    for script in [
        "SELECT city FROM sales WHERE mean(price) > 30 GROUP BY city;",
        "SELECT city, count(*) AS n FROM sales GROUP BY city ORDER BY count(*);",
        "DELETE FROM sales WHERE count(*) > 1 LIMIT ALL;",
    ] {
        let refused = session.run(script);
        assert!(
            matches!(refused, Err(Error::Script(_))),
            "{script} was accepted: {refused:?}"
        );
    }
}

#[test]
fn a_projection_that_is_neither_a_key_nor_a_fold_is_still_refused() {
    // The rule that had to get looser without getting loose. `clerk` has as many
    // values as the group has records, and picking one silently is how a wrong
    // number reaches a report.
    let store = store();
    let mut session = ready(&store);
    let refused = session.run("SELECT clerk, count(*) AS n FROM sales GROUP BY city;");
    assert!(
        matches!(refused, Err(Error::Script(_))),
        "an ungrouped projection was accepted: {refused:?}"
    );
    // …and an expression *built from* a key and a fold is admitted.
    assert!(
        session
            .run("SELECT city, count(*) * 10 AS scaled FROM sales GROUP BY city;")
            .is_ok()
    );
}
