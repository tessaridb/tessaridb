//! A value that reaches the store as a value.
//!
//! Before this existed there was exactly one way to get a variable into a
//! statement: write it into the script text. That makes every caller who has a
//! value build a string, and a caller who builds a string is one quoting mistake
//! away from having built a *statement*.
//!
//! The rule these tests hold to is one sentence: **a parameter is legal exactly
//! where a literal is, and its contents can never become syntax.** The second
//! half is not a promise about escaping — binding happens after parsing, so
//! there is no stage left at which a bound value could be read as grammar. The
//! adversarial case below is what that looks like from outside.

#![allow(clippy::panic, clippy::unwrap_used, clippy::indexing_slicing)]

use std::sync::Arc;

use bgv_db_kv::{KvBackend, MemoryBackend};
use bgv_db_session::{AccessPath, Outcome, Parameters, Session};
use bgv_db_storage::Store;
use bgv_db_types::{Number, RecordId, Value};

fn store() -> Store {
    let backend = Arc::new(MemoryBackend::new()) as Arc<dyn KvBackend>;
    Store::open(backend).unwrap()
}

/// Two users, and a table to prove is still standing afterwards.
fn ready(store: &Store) -> Session<'_> {
    let mut session = Session::new(store);
    session
        .run(
            "DEFINE NAMESPACE prod; USE NAMESPACE prod;\n\
             DEFINE DATABASE shop; USE DATABASE shop;\n\
             DEFINE TABLE users;\n\
             CREATE users:1 = { name: 'ada', city: 'london', age: 36 };\n\
             CREATE users:2 = { name: 'grace', city: 'new york', age: 45 };",
        )
        .unwrap();
    session
}

/// One binding.
fn bound(name: &str, value: Value) -> Parameters {
    let mut parameters = Parameters::new();
    parameters.insert(name.to_owned(), value);
    parameters
}

/// The rows and the path one statement answered with.
fn answered(
    session: &mut Session<'_>,
    script: &str,
    parameters: &Parameters,
) -> (Vec<(RecordId, Value)>, AccessPath) {
    let outcomes = session.run_with(script, parameters).unwrap();
    let last = outcomes.last().unwrap();
    match last {
        Outcome::Records { records, path } => (records.clone(), *path),
        other => panic!("not records: {other:?}"),
    }
}

#[test]
fn a_parameter_reads_as_the_value_it_is_bound_to() {
    let store = store();
    let mut session = ready(&store);
    let (rows, _) = answered(
        &mut session,
        "SELECT * FROM users WHERE name = $who;",
        &bound("who", Value::String("ada".to_owned())),
    );
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].0, RecordId::Int(1));
}

#[test]
fn a_parameter_holding_a_statement_is_a_string() {
    // The whole feature in one test. The bound text reads as a value being
    // compared against, so it matches nothing and removes nothing — and the
    // table it names is still there afterwards, which is the half a caller
    // actually cares about.
    let store = store();
    let mut session = ready(&store);
    let attack = "ada'; DELETE FROM users WHERE age > 0; --";

    let (rows, _) = answered(
        &mut session,
        "SELECT * FROM users WHERE name = $who;",
        &bound("who", Value::String(attack.to_owned())),
    );
    assert!(rows.is_empty(), "the text matched a name: {rows:?}");

    let (survivors, _) = answered(&mut session, "SELECT * FROM users;", &Parameters::new());
    assert_eq!(survivors.len(), 2, "the bound text ran as a statement");
}

#[test]
fn an_unbound_parameter_refuses_the_whole_script() {
    // The refusal happens before the first statement runs, so a script that
    // names an unbound parameter in its last statement writes nothing at all —
    // the alternative leaves a half-applied script behind.
    let store = store();
    let mut session = ready(&store);
    let refusal = session
        .run_with(
            "CREATE users:3 = { name: 'alan' };\n\
             SELECT * FROM users WHERE city = $where;",
            &Parameters::new(),
        )
        .expect_err("an unbound parameter was accepted");
    assert!(
        refusal.to_string().contains("where"),
        "the refusal does not name the parameter: {refusal}"
    );

    let (rows, _) = answered(&mut session, "SELECT * FROM users;", &Parameters::new());
    assert_eq!(rows.len(), 2, "the first statement of a refused script ran");
}

#[test]
fn a_binding_nobody_used_is_accepted() {
    // A caller who sends a map for a script that stopped using one entry has not
    // made a mistake this store can see.
    let store = store();
    let mut session = ready(&store);
    let (rows, _) = answered(
        &mut session,
        "SELECT * FROM users;",
        &bound("unused", Value::Number(Number::Integer(1))),
    );
    assert_eq!(rows.len(), 2);
}

#[test]
fn a_parameter_stands_wherever_a_literal_does() {
    let store = store();
    let mut session = ready(&store);
    let mut parameters = Parameters::new();
    parameters.insert("name".to_owned(), Value::String("alan".to_owned()));
    parameters.insert("age".to_owned(), Value::Number(Number::Integer(41)));
    parameters.insert("city".to_owned(), Value::String("london".to_owned()));

    // A value position: the whole content of a write, built from an object
    // literal whose fields are parameters.
    session
        .run_with(
            "CREATE users:3 = { name: $name, age: $age, tags: [$city, 'x'] };",
            &parameters,
        )
        .unwrap();

    // An argument position, and an arithmetic operand.
    let (rows, _) = answered(
        &mut session,
        "SELECT * FROM users WHERE string::len(name) = 4 AND age + 1 > $age;",
        &parameters,
    );
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].0, RecordId::Int(3));
}

#[test]
fn a_parameter_keeps_the_kind_it_was_bound_as() {
    // The reason the binding is a `Value` and not a string: `36` and `'36'` are
    // different questions, and a caller must not have to know how this store
    // would have parsed the text.
    let store = store();
    let mut session = ready(&store);

    let (matched, _) = answered(
        &mut session,
        "SELECT * FROM users WHERE age = $age;",
        &bound("age", Value::Number(Number::Integer(36))),
    );
    assert_eq!(matched.len(), 1);

    let (as_text, _) = answered(
        &mut session,
        "SELECT * FROM users WHERE age = $age;",
        &bound("age", Value::String("36".to_owned())),
    );
    assert!(as_text.is_empty(), "a string matched a number: {as_text:?}");
}

#[test]
fn an_index_still_serves_a_parameterised_equality() {
    // Binding replaces the parameter before anything plans the read, so the
    // planner sees the literal it would have seen anyway. Without that, every
    // parameterised read — which is every read a real client makes — would
    // quietly fall back to a scan.
    let store = store();
    let mut session = ready(&store);
    session
        .run("DEFINE INDEX by_name ON users FIELDS name UNIQUE;")
        .unwrap();

    let (rows, path) = answered(
        &mut session,
        "SELECT * FROM users WHERE name = $who;",
        &bound("who", Value::String("grace".to_owned())),
    );
    assert_eq!(rows.len(), 1);
    assert_eq!(path, AccessPath::Index, "a parameter lost the index");
}

#[test]
fn run_without_parameters_is_run_with_none() {
    // The common case keeps its one-argument spelling, and the two entry points
    // are the same code — asserted rather than assumed, because a second path
    // is how two spellings start disagreeing.
    let store = store();
    let mut session = ready(&store);
    let (plain, _) = answered(&mut session, "SELECT * FROM users;", &Parameters::new());
    let outcomes = session.run("SELECT * FROM users;").unwrap();
    let Outcome::Records { records, .. } = &outcomes[0] else {
        panic!("not records: {:?}", outcomes[0]);
    };
    assert_eq!(&plain, records);
}

#[test]
fn a_parameter_in_a_stored_expression_is_refused_where_it_is_written() {
    // A `DEFAULT` is evaluated on every write that omits the field, so it
    // belongs to no call and nobody could bind it. Refused at the declaration —
    // which is checked as it is made — rather than surprising a write later.
    let store = store();
    let mut session = ready(&store);
    let refusal = session
        .run_with(
            "DEFINE FIELD city ON users TYPE string DEFAULT $where;",
            &bound("where", Value::String("london".to_owned())),
        )
        .expect_err("a parameter was stored in a default");
    assert!(
        refusal.to_string().contains("belongs to no call"),
        "{refusal}"
    );
}
