//! `UPSERT`, and `MERGE` as the third edit shape.
//!
//! The three write verbs differ in what they assert about the record *before*
//! the write: `CREATE` says it is absent, `UPDATE` says it is present, `UPSERT`
//! says nothing. That is the whole distinction, and keeping the first two is
//! what makes the third safe to add — a caller who knows which case they are in
//! keeps the refusal that tells them when they were wrong.
//!
//! `MERGE` folds an object into the record and leaves what it does not name.
//! Deep where both sides hold an object, and the incoming value whole
//! everywhere else. The tests below pin the boundary between those two rules,
//! because that boundary is the whole of the semantics.

#![allow(clippy::panic, clippy::unwrap_used, clippy::indexing_slicing)]

use std::sync::Arc;

use tessari_kv::{KvBackend, MemoryBackend};
use tessari_ql::Parameters;
use tessari_session::Session;
use tessari_storage::Store;
use tessari_types::{Number, Value};

fn store() -> Store {
    let backend = Arc::new(MemoryBackend::new()) as Arc<dyn KvBackend>;
    Store::open(backend).unwrap()
}

fn ready(store: &Store) -> Session<'_> {
    let mut session = Session::new(store);
    session
        .run(
            "DEFINE NAMESPACE prod; USE NAMESPACE prod;\n\
             DEFINE DATABASE shop; USE DATABASE shop;\n\
             DEFINE COLLECTION users;\n\
             CREATE users:1 = { name: 'ada', visits: 3, \
             address: { city: 'London', street: 'Dean' } };",
        )
        .unwrap();
    session
}

/// The record as it stands, read back through the language.
fn record(session: &mut Session<'_>, statement: &str) -> Value {
    let outcome = session.run(statement).unwrap();
    let records = outcome
        .last()
        .expect("an outcome")
        .records()
        .expect("records");
    assert_eq!(records.len(), 1, "expected one record, got {records:?}");
    records[0].1.clone()
}

fn field<'a>(value: &'a Value, name: &str) -> &'a Value {
    let Value::Object(fields) = value else {
        panic!("not an object: {value:?}");
    };
    fields
        .get(name)
        .unwrap_or_else(|| panic!("no {name} in {fields:?}"))
}

fn text(value: &Value) -> &str {
    match value {
        Value::String(held) => held,
        other => panic!("not a string: {other:?}"),
    }
}

// ---------------------------------------------------------------- UPSERT

#[test]
fn upsert_writes_a_record_that_is_not_there() {
    let store = store();
    let mut session = ready(&store);

    session.run("UPSERT users:9 = { name: 'grace' };").unwrap();

    let held = record(&mut session, "SELECT * FROM users WHERE name = 'grace';");
    assert_eq!(text(field(&held, "name")), "grace");
}

#[test]
fn upsert_replaces_a_record_that_is_there() {
    let store = store();
    let mut session = ready(&store);

    session.run("UPSERT users:1 = { name: 'grace' };").unwrap();

    let held = record(&mut session, "SELECT * FROM users WHERE name = 'grace';");
    let Value::Object(fields) = &held else {
        panic!("not an object: {held:?}");
    };
    assert!(
        !fields.contains_key("visits"),
        "a whole-value write replaces, so the old fields are gone: {fields:?}"
    );
}

#[test]
fn upsert_set_over_an_absent_record_writes_what_it_names() {
    let store = store();
    let mut session = ready(&store);

    // The record starts as an empty object, so `SET` produces exactly the routes
    // it assigns — no special case, and no refusal for a route that "is not
    // there" when nothing is.
    session.run("UPSERT users:9 SET name = 'grace';").unwrap();

    let held = record(&mut session, "SELECT * FROM users WHERE name = 'grace';");
    assert_eq!(text(field(&held, "name")), "grace");
}

#[test]
fn upsert_set_over_a_present_record_changes_only_what_it_names() {
    let store = store();
    let mut session = ready(&store);

    session.run("UPSERT users:1 SET visits = 4;").unwrap();

    let held = record(&mut session, "SELECT * FROM users WHERE name = 'ada';");
    assert_eq!(field(&held, "visits"), &Value::Number(Number::Integer(4)));
}

#[test]
fn create_and_update_keep_their_refusals() {
    let store = store();
    let mut session = ready(&store);

    // The reason `UPSERT` is a third verb rather than a flag: these two still
    // say what they assert, and still refuse when it is untrue.
    assert!(
        session.run("CREATE users:1 = { name: 'x' };").is_err(),
        "CREATE over an existing record must still be refused"
    );
    assert!(
        session.run("UPDATE users:9 = { name: 'x' };").is_err(),
        "UPDATE over an absent record must still be refused"
    );
}

// ----------------------------------------------------------------- MERGE

#[test]
fn merge_leaves_what_it_does_not_name() {
    let store = store();
    let mut session = ready(&store);

    session.run("UPDATE users:1 MERGE { visits: 4 };").unwrap();

    let held = record(&mut session, "SELECT * FROM users WHERE name = 'ada';");
    assert_eq!(field(&held, "visits"), &Value::Number(Number::Integer(4)));
    assert_eq!(
        text(field(&held, "name")),
        "ada",
        "a field the merge did not name is untouched"
    );
}

#[test]
fn merge_is_deep_where_both_sides_hold_an_object() {
    let store = store();
    let mut session = ready(&store);

    session
        .run("UPDATE users:1 MERGE { address: { city: 'Paris' } };")
        .unwrap();

    let held = record(&mut session, "SELECT * FROM users WHERE name = 'ada';");
    let address = field(&held, "address");
    assert_eq!(text(field(address, "city")), "Paris");
    assert_eq!(
        text(field(address, "street")),
        "Dean",
        "the nested field the merge did not name survives — this is the whole \
         difference from a whole-value write"
    );
}

#[test]
fn merge_replaces_rather_than_folding_when_a_side_is_not_an_object() {
    let store = store();
    let mut session = ready(&store);

    session
        .run("UPDATE users:1 MERGE { address: 'unknown' };")
        .unwrap();

    let held = record(&mut session, "SELECT * FROM users WHERE name = 'ada';");
    assert_eq!(
        text(field(&held, "address")),
        "unknown",
        "a scalar over an object is the incoming value whole"
    );
}

#[test]
fn merge_adds_a_route_the_record_did_not_have() {
    let store = store();
    let mut session = ready(&store);

    // Unlike `SET a.b = 1`, which refuses a missing intermediate rather than
    // writing structure nobody asked for. `MERGE` is handed the structure, so
    // there is nothing to invent.
    session
        .run("UPDATE users:1 MERGE { billing: { plan: 'pro' } };")
        .unwrap();

    let held = record(&mut session, "SELECT * FROM users WHERE name = 'ada';");
    assert_eq!(text(field(field(&held, "billing"), "plan")), "pro");
}

#[test]
fn the_merged_object_stands_in_the_value_position() {
    let store = store();
    let mut session = ready(&store);

    // A bare name inside it is a **table**, as it is inside every other object
    // literal in this language — so this reads as "set `visits` to the table
    // `visits` plus one", and there is no such table. Computing from the record
    // is what `SET` is for, and `{ a: b }` meaning two things depending on the
    // verb before it would be worse than this refusal.
    assert!(
        session
            .run("UPDATE users:1 MERGE { visits: visits + 1 };")
            .is_err()
    );

    session
        .run("UPDATE users:1 SET visits = visits + 1;")
        .unwrap();
    let held = record(&mut session, "SELECT * FROM users WHERE name = 'ada';");
    assert_eq!(field(&held, "visits"), &Value::Number(Number::Integer(4)));
}

#[test]
fn a_merge_takes_its_object_from_a_parameter() {
    let store = store();
    let mut session = ready(&store);

    // The shape a PATCH handler actually holds: an object that arrived from
    // outside, folded in whole.
    let mut parameters = Parameters::new();
    parameters.insert(
        "patch".to_owned(),
        Value::Object(
            [("visits".to_owned(), Value::Number(Number::Integer(9)))]
                .into_iter()
                .collect(),
        ),
    );
    session
        .run_with("UPDATE users:1 MERGE $patch;", &parameters)
        .unwrap();

    let held = record(&mut session, "SELECT * FROM users WHERE name = 'ada';");
    assert_eq!(field(&held, "visits"), &Value::Number(Number::Integer(9)));
    assert_eq!(
        text(field(&held, "name")),
        "ada",
        "and the fields the patch did not name are left alone"
    );
}

#[test]
fn merge_needs_an_object() {
    let store = store();
    let mut session = ready(&store);

    // `MERGE 3` could only mean "the record becomes 3", and `UPDATE t:1 = 3`
    // already says that.
    assert!(session.run("UPDATE users:1 MERGE 3;").is_err());
    assert!(session.run("UPDATE users:1 MERGE [1, 2];").is_err());
}

#[test]
fn upsert_merge_over_an_absent_record_produces_what_it_names() {
    let store = store();
    let mut session = ready(&store);

    session
        .run("UPSERT users:9 MERGE { name: 'grace', address: { city: 'Paris' } };")
        .unwrap();

    let held = record(&mut session, "SELECT * FROM users WHERE name = 'grace';");
    assert_eq!(text(field(field(&held, "address"), "city")), "Paris");
}
