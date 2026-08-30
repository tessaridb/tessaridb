//! What a `FROM` may name: a table, a table under another name, and a read.
//!
//! Three things that look like three features and are one. A row files each of
//! its two sides under a name; a table brings one and a read brings none; so a
//! read joined to anything needs `AS`, and once `AS` exists a table can be
//! joined to itself, which it never could before.
//!
//! The rule that is not a convenience is the ceiling. A materialised source has
//! no index to walk and no bound to push into it, so every record it answers
//! with is held at once. It therefore states how much it may hold, rather than
//! being cut at a number nobody wrote — a truncated source answers a different
//! question from the one that was asked and looks exactly like a complete one.

#![allow(clippy::panic, clippy::unwrap_used, clippy::indexing_slicing)]

use std::sync::Arc;

use tessari_kv::{KvBackend, MemoryBackend};
use tessari_session::{Outcome, Parameters, Session};
use tessari_storage::Store;
use tessari_types::{RecordId, Value};

fn store() -> Store {
    let backend = Arc::new(MemoryBackend::new()) as Arc<dyn KvBackend>;
    Store::open(backend).unwrap()
}

/// Two users, one reporting to the other, and two orders.
fn ready(store: &Store) -> Session<'_> {
    let mut session = Session::new(store);
    session
        .run(
            "DEFINE NAMESPACE prod; USE NAMESPACE prod;\n\
             DEFINE DATABASE shop; USE DATABASE shop;\n\
             DEFINE COLLECTION users; DEFINE COLLECTION orders;\n\
             CREATE users:1 = { name: 'ada', code: 1 };\n\
             CREATE users:2 = { name: 'grace', code: 2, boss: 1 };\n\
             CREATE orders:1 = { who: 'ada', total: 3 };\n\
             CREATE orders:2 = { who: 'grace', total: 7 };",
        )
        .unwrap();
    session
}

fn rows(session: &mut Session<'_>, script: &str) -> Vec<(RecordId, Value)> {
    match session.run(script).unwrap().last().unwrap() {
        Outcome::Records { records, .. } => records.clone(),
        other => panic!("not records: {other:?}"),
    }
}

fn side<'a>(row: &'a Value, name: &str) -> &'a Value {
    let Value::Object(fields) = row else {
        panic!("not a row: {row:?}");
    };
    fields
        .get(name)
        .unwrap_or_else(|| panic!("no `{name}` side in {fields:?}"))
}

fn field<'a>(value: &'a Value, name: &str) -> &'a Value {
    let Value::Object(fields) = value else {
        panic!("not an object: {value:?}");
    };
    fields
        .get(name)
        .unwrap_or_else(|| panic!("no {name} in {fields:?}"))
}

#[test]
fn an_alias_names_the_side_the_row_files_it_under() {
    let store = store();
    let mut session = ready(&store);

    let found = rows(
        &mut session,
        "SELECT * FROM users AS u JOIN orders AS o ON u.name = o.who;",
    );
    assert_eq!(found.len(), 2);

    let Value::Object(row) = &found[0].1 else {
        panic!("not a row");
    };
    assert!(row.contains_key("u") && row.contains_key("o"), "{row:?}");
    assert!(
        !row.contains_key("users"),
        "the table name outlived the alias: {row:?}"
    );
}

#[test]
fn a_table_may_be_joined_to_itself_under_two_names() {
    // The join aliases exist for. Without them the two sides of a self-join
    // share one name and the row has no side a reader could address.
    let store = store();
    let mut session = ready(&store);

    let found = rows(
        &mut session,
        "SELECT * FROM users AS person JOIN users AS boss ON person.boss = boss.code;",
    );
    assert_eq!(found.len(), 1, "grace reports to ada, and nobody else does");
    assert_eq!(
        field(side(&found[0].1, "person"), "name"),
        &Value::from("grace")
    );
    assert_eq!(
        field(side(&found[0].1, "boss"), "name"),
        &Value::from("ada")
    );
}

#[test]
fn two_sides_under_one_name_are_still_refused() {
    // The check moved from the table to the name, which is what made the
    // self-join above sayable. It did not go away: two sides that answer under
    // one name are a row with one half.
    let store = store();
    let mut session = ready(&store);

    assert!(
        session
            .run("SELECT * FROM users JOIN users ON users.boss = users.code;")
            .is_err(),
        "a self-join without aliases has one name for both sides"
    );
    assert!(
        session
            .run("SELECT * FROM users AS x JOIN orders AS x ON x.name = x.who;")
            .is_err(),
        "two different tables under one name is the same row with one half"
    );
}

#[test]
fn a_name_given_where_there_is_no_join_is_refused() {
    // Accepting it and ignoring it would be the quieter choice and the wrong
    // one: a reader who wrote a name expects to be able to use it.
    let store = store();
    let mut session = ready(&store);

    assert!(session.run("SELECT * FROM users AS u;").is_err());
}

#[test]
fn a_read_stands_as_a_source() {
    // The answer is the inner records themselves — not wrapped, not renamed —
    // so the outer statement reads them exactly as it would read a table.
    let store = store();
    let mut session = ready(&store);

    let found = rows(
        &mut session,
        "SELECT * FROM (SELECT * FROM orders WHERE total > 5 LIMIT 10);",
    );
    assert_eq!(found.len(), 1);
    assert_eq!(field(&found[0].1, "who"), &Value::from("grace"));
}

#[test]
fn the_outer_statement_shapes_what_the_inner_one_answered() {
    let store = store();
    let mut session = ready(&store);

    let found = rows(
        &mut session,
        "SELECT who FROM (SELECT * FROM orders LIMIT 10) WHERE total > 5;",
    );
    assert_eq!(found.len(), 1);
    let Value::Object(record) = &found[0].1 else {
        panic!("not an object");
    };
    assert_eq!(record.len(), 1, "the outer projection did not apply");
    assert_eq!(record.get("who"), Some(&Value::from("grace")));
}

#[test]
fn a_source_read_must_state_its_ceiling() {
    // The rule that is not a convenience. A materialised source holds every
    // record it answers with, so one that could grow without limit is refused
    // rather than cut at a number nobody wrote.
    let store = store();
    let mut session = ready(&store);

    let refused = session
        .run("SELECT * FROM (SELECT * FROM orders);")
        .expect_err("a refusal");
    assert!(refused.to_string().contains("LIMIT"), "{refused}");
}

#[test]
fn a_read_stands_on_either_side_of_a_join() {
    let store = store();
    let mut session = ready(&store);

    let left = rows(
        &mut session,
        "SELECT * FROM (SELECT * FROM users LIMIT 10) AS u \
         JOIN orders AS o ON u.name = o.who;",
    );
    assert_eq!(left.len(), 2);
    assert_eq!(field(side(&left[0].1, "u"), "name"), &Value::from("ada"));

    let right = rows(
        &mut session,
        "SELECT * FROM users AS u \
         JOIN (SELECT * FROM orders WHERE total > 5 LIMIT 10) AS o ON u.name = o.who;",
    );
    assert_eq!(
        right.len(),
        1,
        "the inner condition did not narrow the side"
    );
    assert_eq!(field(side(&right[0].1, "o"), "total"), &Value::from(7_i64));
}

#[test]
fn a_joined_read_must_name_itself() {
    let store = store();
    let mut session = ready(&store);

    assert!(
        session
            .run(
                "SELECT * FROM users AS u \
                 JOIN (SELECT * FROM orders LIMIT 10) ON u.name = o.who;"
            )
            .is_err(),
        "a read has no name of its own, so the row has no side to file it under"
    );
    assert!(
        session
            .run(
                "SELECT * FROM (SELECT * FROM users LIMIT 10) \
                 JOIN orders AS o ON u.name = o.who;"
            )
            .is_err()
    );
}

#[test]
fn a_joined_read_states_its_ceiling_too() {
    let store = store();
    let mut session = ready(&store);

    let refused = session
        .run("SELECT * FROM users AS u JOIN (SELECT * FROM orders) AS o ON u.name = o.who;")
        .expect_err("a refusal");
    assert!(refused.to_string().contains("LIMIT"), "{refused}");
}

#[test]
fn a_parameter_still_reaches_inside_a_materialised_read() {
    // Binding substitutes forward into the parsed tree, so a name inside a
    // subquery source is a literal by the time the planner reads it — the same
    // property that lets a bound name reach an index anywhere else.
    let store = store();
    let mut session = ready(&store);

    let mut parameters = Parameters::new();
    parameters.insert("floor".to_owned(), Value::from(5_i64));
    let outcomes = session
        .run_with(
            "SELECT * FROM (SELECT * FROM orders WHERE total > $floor LIMIT 10);",
            &parameters,
        )
        .unwrap();
    let Outcome::Records { records, .. } = outcomes.last().unwrap() else {
        panic!("not records");
    };
    assert_eq!(records.len(), 1);
}

#[test]
fn a_condition_over_a_materialised_read_asks_about_what_it_produced() {
    // The shape no condition *inside* the read could have taken: `n` is the
    // fold's answer, and it does not exist until the group is folded. Wrapping
    // the read is what gives the question somewhere to stand.
    let store = store();
    let mut session = ready(&store);
    session
        .run("CREATE orders:3 = { who: 'ada', total: 1 };")
        .unwrap();

    let found = rows(
        &mut session,
        "SELECT * FROM (SELECT who, count(*) AS n FROM orders GROUP BY who LIMIT 10) \
         WHERE n > 1;",
    );
    assert_eq!(found.len(), 1, "only ada has more than one order");
    assert_eq!(field(&found[0].1, "who"), &Value::from("ada"));
}

#[test]
fn a_traversal_becomes_filterable_by_being_materialised() {
    // `WHERE` belongs to the table position, so a walk had nowhere to put one.
    // This is the whole of the fix, and it needed no clause of its own.
    let store = store();
    let mut session = ready(&store);
    session
        .run(
            "DEFINE TABLE follows EDGE;\n\
             RELATE users:1 -> follows -> users:2;",
        )
        .unwrap();

    let found = rows(
        &mut session,
        "SELECT * FROM (SELECT * FROM users:1->follows->users LIMIT 10) WHERE code = 2;",
    );
    assert_eq!(found.len(), 1);
    assert_eq!(field(&found[0].1, "name"), &Value::from("grace"));
}
