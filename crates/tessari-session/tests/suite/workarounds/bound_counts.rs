//! `LIMIT $n`, `START $n` and `CLAIM $n FROM q` (ADR-0124 D5).

use std::collections::BTreeMap;

use tessari_session::{Error, Outcome, Parameters};
use tessari_types::{Number, Value};

use super::{inside, refused, run, store};

fn count(outcome: &Outcome) -> usize {
    match outcome {
        Outcome::Records { records, .. } => records.len(),
        other => panic!("{other:?}"),
    }
}

fn bound(name: &str, value: Value) -> Parameters {
    BTreeMap::from([(name.to_owned(), value)])
}

fn int(n: i64) -> Value {
    Value::Number(Number::Integer(n))
}

#[test]
fn a_bound_limit_and_start_answer_what_the_literals_answer() {
    let store = store();
    let mut session = inside(&store);
    run(
        &mut session,
        "DEFINE TABLE t SCHEMALESS; CREATE t:1 = {}; CREATE t:2 = {}; CREATE t:3 = {}; \
         CREATE t:4 = {}; CREATE t:5 = {};",
    );
    let literal = run(&mut session, "SELECT * FROM t START 1 LIMIT 2;");
    let mut parameters = bound("n", int(2));
    parameters.insert("s".to_owned(), int(1));
    let with = session
        .run_with("SELECT * FROM t START $s LIMIT $n;", &parameters)
        .unwrap()
        .pop()
        .unwrap();
    assert_eq!(count(&with), 2);
    assert_eq!(with, literal);
    let lets = session
        .run("LET $n = 3; SELECT * FROM t LIMIT $n;")
        .unwrap()
        .pop()
        .unwrap();
    assert_eq!(count(&lets), 3);
}

#[test]
fn a_bound_count_that_is_not_a_whole_number_is_refused() {
    let store = store();
    let mut session = inside(&store);
    run(&mut session, "DEFINE TABLE t SCHEMALESS;");
    for wrong in [Value::from("2"), int(-1), Value::Number(Number::Float(1.5))] {
        let error = session
            .run_with("SELECT * FROM t LIMIT $n;", &bound("n", wrong.clone()))
            .unwrap_err();
        assert!(
            error.to_string().contains("`$n` is bound to"),
            "{wrong:?}: {error}"
        );
    }
}

#[test]
fn a_bound_claim_takes_that_many_and_keeps_its_ceiling() {
    let store = store();
    let mut session = inside(&store);
    run(
        &mut session,
        "DEFINE QUEUE jobs TIMEOUT 30s; CREATE jobs = { i: 1 }; CREATE jobs = { i: 2 }; \
         CREATE jobs = { i: 3 };",
    );
    let taken = session
        .run_with("CLAIM $n FROM jobs;", &bound("n", int(2)))
        .unwrap()
        .pop()
        .unwrap();
    assert_eq!(count(&taken), 2);
    let literal = refused(&mut session, "CLAIM 501 FROM jobs;");
    let with = session
        .run_with("CLAIM $n FROM jobs;", &bound("n", int(501)))
        .unwrap_err();
    assert!(
        matches!(literal, Error::ClaimAboveCeiling { .. }),
        "{literal:?}"
    );
    assert!(matches!(with, Error::ClaimAboveCeiling { .. }), "{with:?}");
    let zero = session
        .run_with("CLAIM $n FROM jobs;", &bound("n", int(0)))
        .unwrap_err();
    assert!(zero.to_string().contains("`$n` is bound to"), "{zero}");
}
