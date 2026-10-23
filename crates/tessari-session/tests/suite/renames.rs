//! `ALTER TABLE … RENAME TO` and `RENAME FIELD … TO` (ADR-0134 D9).

#![allow(clippy::panic, clippy::unwrap_used, clippy::indexing_slicing)]

use std::sync::Arc;
use tessari_session::Now;
use tessari_session::RunNow;

use tessari_kv::{KvBackend, MemoryBackend};
use tessari_session::{Outcome, Session};
use tessari_storage::Store;
use tessari_types::Value;

fn store() -> Store {
    let backend = Arc::new(MemoryBackend::new()) as Arc<dyn KvBackend>;
    Store::open(backend).now().unwrap()
}

/// A declared table of two orders with an index on `who`.
fn ready(store: &Store) -> Session<'_> {
    let mut session = Session::new(store);
    session
        .run_now(
            "DEFINE NAMESPACE prod; USE NAMESPACE prod;\n\
             DEFINE DATABASE shop; USE DATABASE shop;\n\
             DEFINE TABLE orders (who string REQUIRED, total int);\n\
             DEFINE INDEX by_who ON orders FIELDS who;\n\
             CREATE orders:1 = { who: 'ada', total: 3 };\n\
             CREATE orders:2 = { who: 'grace', total: 7 };",
        )
        .unwrap();
    session
}

fn rows(session: &mut Session<'_>, statement: &str) -> Vec<Value> {
    match session.run_now(statement).unwrap().pop().unwrap() {
        Outcome::Records { records, .. } => records.into_iter().map(|(_, row)| row).collect(),
        other => panic!("records, got {other:?}"),
    }
}

fn refused(session: &mut Session<'_>, statement: &str) -> String {
    session.run_now(statement).unwrap_err().to_string()
}

#[test]
fn a_renamed_table_keeps_its_records_and_its_index() {
    let store = store();
    let mut session = ready(&store);

    session
        .run_now("ALTER TABLE orders RENAME TO purchases;")
        .unwrap();

    assert_eq!(
        rows(
            &mut session,
            "SELECT * FROM purchases WHERE who = 'ada' USING INDEX by_who;"
        )
        .len(),
        1
    );
    assert_eq!(rows(&mut session, "SELECT * FROM purchases;").len(), 2);
    let gone = refused(&mut session, "SELECT * FROM orders;");
    assert!(gone.contains("orders"), "{gone}");
    // The old name is free again.
    session.run_now("DEFINE COLLECTION orders;").unwrap();
}

#[test]
fn a_rename_onto_a_name_taken_is_refused() {
    let store = store();
    let mut session = ready(&store);
    session.run_now("DEFINE COLLECTION archive;").unwrap();

    let taken = refused(&mut session, "ALTER TABLE orders RENAME TO archive;");

    assert!(taken.contains("archive"), "{taken}");
    assert_eq!(rows(&mut session, "SELECT * FROM orders;").len(), 2);
}

#[test]
fn a_table_a_view_or_an_event_names_is_not_renamed() {
    let store = store();
    let mut session = ready(&store);
    session
        .run_now("DEFINE VIEW big AS SELECT * FROM orders WHERE total > 5;")
        .unwrap();

    let named = refused(&mut session, "ALTER TABLE orders RENAME TO purchases;");
    assert!(named.contains("view `big`"), "{named}");

    session.run_now("DROP VIEW big;").unwrap();
    session
        .run_now(
            "DEFINE COLLECTION log;\n\
             DEFINE EVENT touch ON log THEN UPDATE orders:1 SET total = 0;",
        )
        .unwrap();
    let named = refused(&mut session, "ALTER TABLE orders RENAME TO purchases;");
    assert!(named.contains("event `touch` on `log`"), "{named}");
}

#[test]
fn a_bucket_is_not_renamed() {
    let store = store();
    let mut session = ready(&store);
    session.run_now("DEFINE BUCKET media;").unwrap();

    let held = refused(&mut session, "ALTER TABLE media RENAME TO files;");

    assert!(held.contains("`media` cannot be renamed"), "{held}");
}

#[test]
fn a_renamed_field_moves_in_every_record_and_in_the_declaration() {
    let store = store();
    let mut session = ready(&store);

    session
        .run_now("ALTER TABLE orders RENAME FIELD total TO amount;")
        .unwrap();

    let held = rows(&mut session, "SELECT * FROM orders:1;");
    let Value::Object(fields) = &held[0] else {
        panic!("a record");
    };
    assert!(
        fields.contains_key("amount") && !fields.contains_key("total"),
        "{fields:?}"
    );
    // The declared table now knows `amount`, and `total` is undeclared.
    session
        .run_now("CREATE orders:3 = { who: 'alan', amount: 1 };")
        .unwrap();
    let undeclared = refused(&mut session, "CREATE orders:4 = { who: 'alan', total: 1 };");
    assert!(undeclared.contains("total"), "{undeclared}");
}

#[test]
fn a_required_field_is_renamed_over_the_records_holding_it() {
    // The record's rewrite is checked against the table as the commit leaves
    // it, where the requirement has moved to the new name with the field.
    let store = store();
    let mut session = ready(&store);
    session
        .run_now(
            "DEFINE TABLE people (name string REQUIRED);\n\
             CREATE people:1 = { name: 'ada' };\n\
             ALTER TABLE people RENAME FIELD name TO called;",
        )
        .unwrap();

    let held = rows(&mut session, "SELECT called FROM people:1;");
    assert_eq!(
        format!("{held:?}"),
        r#"[Object({"called": String("ada")})]"#
    );
    let missing = refused(&mut session, "CREATE people:2 = {};");
    assert!(missing.contains("called"), "{missing}");
}

#[test]
fn a_field_an_index_is_over_is_not_renamed() {
    let store = store();
    let mut session = ready(&store);

    let indexed = refused(
        &mut session,
        "ALTER TABLE orders RENAME FIELD who TO buyer;",
    );

    assert!(indexed.contains("index `by_who` is over it"), "{indexed}");
}

#[test]
fn a_rename_that_would_overwrite_a_value_is_refused() {
    let store = store();
    let mut session = Session::new(&store);
    session
        .run_now(
            "DEFINE NAMESPACE prod; USE NAMESPACE prod;\n\
             DEFINE DATABASE shop; USE DATABASE shop;\n\
             DEFINE COLLECTION notes;\n\
             CREATE notes:1 = { title: 'a', name: 'b' };",
        )
        .unwrap();

    let overwrites = refused(
        &mut session,
        "ALTER TABLE notes RENAME FIELD title TO name;",
    );

    assert!(
        overwrites.contains("a record already holds `name`"),
        "{overwrites}"
    );
}

#[test]
fn using_converts_every_stored_value_into_the_new_type() {
    let store = store();
    let mut session = ready(&store);

    session
        .run_now("ALTER TABLE orders ALTER FIELD total TYPE string USING type::string(total);")
        .unwrap();

    let held = rows(&mut session, "SELECT total FROM orders:2;");
    let Value::Object(fields) = &held[0] else {
        panic!("a record");
    };
    assert_eq!(fields.get("total"), Some(&Value::from("7")));
    // The new type is the declared one from here on.
    let refused_int = refused(&mut session, "CREATE orders:3 = { who: 'alan', total: 1 };");
    assert!(refused_int.contains("string"), "{refused_int}");
}

#[test]
fn without_using_a_type_the_records_do_not_fit_is_refused() {
    let store = store();
    let mut session = ready(&store);

    let misfit = refused(
        &mut session,
        "ALTER TABLE orders ALTER FIELD total TYPE string;",
    );

    assert!(misfit.contains("total"), "{misfit}");
    let held = rows(&mut session, "SELECT total FROM orders:2;");
    let Value::Object(fields) = &held[0] else {
        panic!("a record");
    };
    assert_eq!(fields.get("total"), Some(&Value::Number(7_i64.into())));
}

#[test]
fn a_conversion_that_does_not_fit_writes_nothing() {
    let store = store();
    let mut session = ready(&store);

    // `total * 2` is still an int, so the string declaration refuses it whole.
    let misfit = refused(
        &mut session,
        "ALTER TABLE orders ALTER FIELD total TYPE string USING total * 2;",
    );

    assert!(misfit.contains("total"), "{misfit}");
    let held = rows(&mut session, "SELECT total FROM orders:2;");
    let Value::Object(fields) = &held[0] else {
        panic!("a record");
    };
    assert_eq!(fields.get("total"), Some(&Value::Number(7_i64.into())));
}
