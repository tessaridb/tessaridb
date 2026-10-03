//! A planner that estimates: `ANALYZE TABLE`, the estimate a plan reports, and
//! the choices an estimate changes.
//!
//! An estimate changes which path a read takes and never what it answers, so
//! every test that moves a path also compares the answer with the unindexed
//! read of the same condition.

#![allow(clippy::panic, clippy::unwrap_used, clippy::indexing_slicing)]

use std::collections::BTreeMap;
use std::sync::Arc;

use tessari_kv::{KvBackend, MemoryBackend};
use tessari_session::{Outcome, Session};
use tessari_storage::Store;
use tessari_types::{Number, RecordId, Value};

fn store() -> Store {
    let backend = Arc::new(MemoryBackend::new()) as Arc<dyn KvBackend>;
    Store::open(backend).unwrap()
}

/// `count` people with `band = n % 10` and `rare = n % 500`, in one table that
/// carries an index on each and a mirror that carries none.
fn ready(store: &Store, count: i64) -> Session<'_> {
    let mut session = Session::new(store);
    session
        .run(
            "DEFINE NAMESPACE prod; USE NAMESPACE prod;\n\
             DEFINE DATABASE shop; USE DATABASE shop;\n\
             DEFINE COLLECTION people; DEFINE COLLECTION mirror;\n\
             DEFINE INDEX by_band ON people FIELDS band;\n\
             DEFINE INDEX by_rare ON people FIELDS rare;\n\
             DEFINE INDEX by_n ON people FIELDS n;\n\
             DEFINE INDEX by_id ON people FIELDS n UNIQUE;",
        )
        .unwrap();
    write(&mut session, 0, count);
    session
}

fn write(session: &mut Session<'_>, from: i64, to: i64) {
    let mut script = String::new();
    for n in from..to {
        let body = format!("{{ band: {}, rare: {}, n: {n} }}", n % 10, n % 500);
        script.push_str(&format!(
            "UPSERT people:{n} = {body}; UPSERT mirror:{n} = {body};\n"
        ));
    }
    session.run(&script).unwrap();
}

fn explained(session: &mut Session<'_>, condition: &str) -> BTreeMap<String, Value> {
    let outcomes = session
        .run(&format!("EXPLAIN SELECT * FROM people WHERE {condition};"))
        .unwrap();
    let Some(Outcome::Value(Value::Object(plan))) = outcomes.last() else {
        panic!("{outcomes:?}");
    };
    plan.clone()
}

fn estimate(plan: &BTreeMap<String, Value>) -> Option<(i64, String)> {
    let Some(Value::Number(Number::Integer(rows))) = plan.get("estimate") else {
        return None;
    };
    let Some(Value::String(by)) = plan.get("estimated_by") else {
        panic!("an estimate without its source: {plan:?}");
    };
    Some((*rows, by.clone()))
}

fn ids(session: &mut Session<'_>, script: &str) -> Vec<RecordId> {
    let outcomes = session.run(script).unwrap();
    outcomes
        .last()
        .unwrap()
        .records()
        .unwrap()
        .iter()
        .map(|(id, _)| id.clone())
        .collect()
}

/// The same condition over the indexed table and over the mirror.
fn agree(session: &mut Session<'_>, rest: &str) {
    let indexed = ids(session, &format!("SELECT * FROM people {rest};"));
    let plain = ids(session, &format!("SELECT * FROM mirror {rest};"));
    assert_eq!(indexed, plain, "`{rest}` answered differently by index");
}

#[test]
fn analyze_answers_each_value_index_it_summarised() {
    let store = store();
    let mut session = ready(&store, 2_000);
    let outcomes = session.run("ANALYZE TABLE people;").unwrap();
    let Some(Outcome::Value(Value::Array(taken))) = outcomes.last() else {
        panic!("{outcomes:?}");
    };
    let named: Vec<&Value> = taken
        .iter()
        .map(|summary| {
            let Value::Object(fields) = summary else {
                panic!("{summary:?}");
            };
            fields.get("index").unwrap()
        })
        .collect();
    // The unique index already names at most one record per value and keeps
    // no statistic.
    assert_eq!(
        named,
        vec![
            &Value::from("by_band"),
            &Value::from("by_rare"),
            &Value::from("by_n")
        ]
    );
    let Value::Object(band) = &taken[0] else {
        panic!()
    };
    assert_eq!(band.get("entries"), Some(&Value::from(2_000_i64)));
    assert_eq!(
        band.get("distinct"),
        Some(&Value::Array(vec![Value::from(10_i64)]))
    );
}

#[test]
fn a_plan_reports_the_probe_before_analyze_and_the_statistic_after() {
    let store = store();
    let mut session = ready(&store, 2_000);
    let before = explained(&mut session, "band = 3");
    assert_eq!(
        estimate(&before),
        Some((200, "probe".to_owned())),
        "{before:?}"
    );
    session.run("ANALYZE TABLE people;").unwrap();
    let after = explained(&mut session, "band = 3");
    assert_eq!(
        estimate(&after),
        Some((200, "statistics".to_owned())),
        "{after:?}"
    );
    agree(&mut session, "WHERE band = 3");
}

#[test]
fn a_statistic_the_index_has_changed_past_is_set_aside() {
    let store = store();
    let mut session = ready(&store, 2_000);
    session.run("ANALYZE TABLE people;").unwrap();
    // Every record moves to another band: two thousand entries change, past
    // both a tenth of the entries and the floor.
    let mut script = String::new();
    for n in 0..2_000 {
        script.push_str(&format!("UPDATE people:{n} SET band = {};\n", (n + 1) % 10));
    }
    session.run(&script).unwrap();
    let plan = explained(&mut session, "band = 3");
    assert_eq!(
        estimate(&plan).map(|(_, by)| by),
        Some("probe".to_owned()),
        "{plan:?}"
    );
}

#[test]
fn an_estimate_ranks_two_indexes_the_shape_could_not_tell_apart() {
    let store = store();
    let mut session = ready(&store, 2_000);
    // Two equalities, each on an index: before statistics the one written first
    // wins, though it selects two hundred records and the other four.
    let condition = "band = 3 AND rare = 13";
    assert_eq!(
        explained(&mut session, condition).get("index"),
        Some(&Value::from("by_band"))
    );
    session.run("ANALYZE TABLE people;").unwrap();
    let plan = explained(&mut session, condition);
    assert_eq!(plan.get("index"), Some(&Value::from("by_rare")), "{plan:?}");
    assert_eq!(estimate(&plan), Some((4, "statistics".to_owned())));
    agree(&mut session, &format!("WHERE {condition}"));
}

#[test]
fn an_index_that_selects_most_of_the_table_loses_to_it_on_the_statistic_alone() {
    let store = store();
    let mut session = ready(&store, 2_000);
    session.run("ANALYZE TABLE people;").unwrap();
    let plan = explained(&mut session, "n > 10");
    assert_eq!(plan.get("access"), Some(&Value::from("scan")), "{plan:?}");
    // And one that selects a few is served on it, with no count taken.
    let narrow = explained(&mut session, "n > 1900");
    assert_eq!(
        narrow.get("index"),
        Some(&Value::from("by_n")),
        "{narrow:?}"
    );
    let (rows, by) = estimate(&narrow).unwrap();
    assert_eq!(by, "statistics");
    assert!((60..=140).contains(&rows), "estimated {rows} of 99");
    agree(&mut session, "WHERE n > 1900");
    agree(&mut session, "WHERE n > 10");
}

#[test]
fn a_serving_node_takes_the_statistics_itself() {
    let store = store();
    let mut session = ready(&store, 2_000);
    assert_eq!(store.refresh_statistics().unwrap(), 3);
    let plan = explained(&mut session, "band = 3");
    assert_eq!(
        estimate(&plan).map(|(_, by)| by),
        Some("statistics".to_owned())
    );
    // Fresh now, so the next pass has nothing to take.
    assert_eq!(store.refresh_statistics().unwrap(), 0);
}

#[test]
fn a_bounded_equality_answers_what_the_scan_answers_inside_a_transaction() {
    let store = store();
    let mut session = ready(&store, 2_000);
    // Moved into the band, moved out of it, and deleted — all uncommitted, on
    // both tables, so the streamed index walk meets its own writes three ways.
    let mut script = String::from("BEGIN;\n");
    for table in ["people", "mirror"] {
        script.push_str(&format!(
            "UPDATE {table}:1 SET band = 3;\n\
             UPDATE {table}:13 SET band = 4;\n\
             DELETE {table}:23;\n\
             CREATE {table}:5000 = {{ band: 3, rare: 0, n: 5000 }};\n"
        ));
    }
    let reads = ["WHERE band = 3 LIMIT 3", "WHERE band = 3"];
    for rest in reads {
        script.push_str(&format!(
            "SELECT * FROM people {rest}; SELECT * FROM mirror {rest};\n"
        ));
    }
    script.push_str("CANCEL;");
    let outcomes = session.run(&script).unwrap();
    let answered: Vec<Vec<RecordId>> = outcomes
        .iter()
        .filter_map(|outcome| outcome.records())
        .map(|records| records.iter().map(|(id, _)| id.clone()).collect())
        .collect();
    assert_eq!(answered.len(), reads.len() * 2, "{outcomes:?}");
    for (pair, rest) in answered.chunks(2).zip(reads) {
        assert_eq!(pair[0], pair[1], "`{rest}` answered differently by index");
    }
    assert_eq!(
        answered[0],
        vec![RecordId::Int(1), RecordId::Int(3), RecordId::Int(33)],
        "the moved-in record first, the moved-out and deleted ones gone"
    );
    assert!(answered[2].contains(&RecordId::Int(5000)));
}

#[test]
fn an_estimate_on_a_field_the_caller_cannot_read_is_withheld() {
    let store = store();
    let mut root = Session::new(&store);
    root.run(
        "DEFINE NAMESPACE prod; USE NAMESPACE prod;\n\
         DEFINE DATABASE shop; USE DATABASE shop;\n\
         DEFINE COLLECTION staff;\n\
         DEFINE INDEX by_salary ON staff FIELDS salary;\n\
         CREATE staff:1 = { name: 'ada', salary: 120000 };\n\
         CREATE staff:2 = { name: 'grace', salary: 200000 };\n\
         DEFINE USER root ROLE owner PASSWORD 'correct horse battery';",
    )
    .unwrap();
    let mut owner = Session::new(&store);
    owner.sign_in("root", "correct horse battery").unwrap();
    owner
        .run(
            "USE NAMESPACE prod; USE DATABASE shop;\n\
             DEFINE USER ada ON prod.shop ROLE editor PASSWORD 'correct horse battery';\n\
             GRANT read ON staff FIELDS name TO ada;\n\
             ANALYZE TABLE staff;",
        )
        .unwrap();
    let read = "EXPLAIN SELECT * FROM staff WHERE salary = 200000;";
    let plan = |session: &mut Session<'_>| {
        let outcomes = session.run(read).unwrap();
        let Some(Outcome::Value(Value::Object(plan))) = outcomes.last() else {
            panic!("{outcomes:?}");
        };
        plan.clone()
    };
    // The owner, who may read the field, is told; otherwise the assertion
    // below passes for a plan that never carries an estimate at all.
    assert!(estimate(&plan(&mut owner)).is_some());
    let mut ada = Session::new(&store);
    ada.sign_in("ada", "correct horse battery").unwrap();
    ada.run("USE NAMESPACE prod; USE DATABASE shop;").unwrap();
    let hidden = plan(&mut ada);
    assert_eq!(hidden.get("index"), Some(&Value::from("by_salary")));
    assert_eq!(estimate(&hidden), None, "{hidden:?}");
}
