//! Running scripts against a real store.
//!
//! These are the tests the parser's could not be: they assert that a statement
//! reaches the store and that the store answers, which is the only evidence that
//! the language does anything at all.

#![allow(clippy::panic, clippy::unwrap_used, clippy::indexing_slicing)]

use std::sync::Arc;

use bgv_db_kv::{KvBackend, MemoryBackend};
use bgv_db_session::{Error, Session};
use bgv_db_storage::Store;
use bgv_db_types::{RecordId, Value};

fn store() -> Store {
    let backend = Arc::new(MemoryBackend::new()) as Arc<dyn KvBackend>;
    Store::open(backend).unwrap()
}

/// A session with `prod / orders` defined and selected.
fn ready(store: &Store) -> Session<'_> {
    let mut session = Session::new(store);
    session
        .run(
            "DEFINE NAMESPACE prod;\n\
             USE NAMESPACE prod;\n\
             DEFINE DATABASE orders;\n\
             USE DATABASE orders;",
        )
        .unwrap();
    session
}

fn field<'a>(value: &'a Value, name: &str) -> &'a Value {
    let Value::Object(object) = value else {
        panic!("not an object: {value:?}");
    };
    object
        .get(name)
        .unwrap_or_else(|| panic!("no field {name}"))
}

#[test]
fn a_script_defines_a_table_and_writes_to_it() {
    let store = store();
    let mut session = ready(&store);

    session
        .run("DEFINE TABLE users; CREATE users:1 = { name: 'ada' };")
        .unwrap();

    let outcomes = session.run("SELECT * FROM users:1;").unwrap();
    let records = outcomes[0].records().unwrap();
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].0, RecordId::Int(1));
    assert_eq!(
        field(&records[0].1, "name"),
        &Value::String("ada".to_owned())
    );
}

#[test]
fn a_definition_and_a_write_land_together_or_not_at_all() {
    // The reason the catalog is records rather than a keyspace of its own: a
    // definition takes part in the transaction that issued it.
    let store = store();
    let mut session = ready(&store);

    let failed = session.run(
        "BEGIN;\n\
         DEFINE TABLE accounts;\n\
         CREATE accounts:1 = { balance: 10 };\n\
         CANCEL;",
    );
    assert!(failed.is_ok(), "{failed:?}");

    // The table is gone with the transaction that defined it, so the read
    // cannot even name it.
    let error = session.run("SELECT * FROM accounts;").unwrap_err();
    assert!(
        matches!(
            &error,
            Error::Unknown {
                entity: "table",
                ..
            }
        ),
        "{error}"
    );

    session
        .run(
            "BEGIN;\n\
             DEFINE TABLE accounts;\n\
             CREATE accounts:1 = { balance: 10 };\n\
             COMMIT;",
        )
        .unwrap();
    let outcomes = session.run("SELECT * FROM accounts;").unwrap();
    assert_eq!(outcomes[0].records().unwrap().len(), 1);
}

#[test]
fn a_script_that_never_commits_discards_its_work_and_says_so() {
    let store = store();
    let mut session = ready(&store);
    session.run("DEFINE TABLE users;").unwrap();

    let error = session
        .run("BEGIN; CREATE users:1 = { name: 'ada' };")
        .unwrap_err();
    assert!(
        matches!(error, Error::UnclosedTransaction { .. }),
        "{error}"
    );

    // Committing silently would commit work the author never said was
    // finished; discarding silently would hide that it ran.
    let outcomes = session.run("SELECT * FROM users;").unwrap();
    assert!(outcomes[0].records().unwrap().is_empty());
}

#[test]
fn the_three_access_paths_answer_from_the_store() {
    let store = store();
    let mut session = ready(&store);
    session
        .run(
            "DEFINE TABLE users;\n\
             DEFINE INDEX by_email ON users FIELDS email UNIQUE;\n\
             CREATE users:1 = { name: 'ada', email: 'ada@example.com' };\n\
             CREATE users:2 = { name: 'grace', email: 'grace@example.com' };",
        )
        .unwrap();

    let by_id = session.run("SELECT * FROM users:1;").unwrap();
    assert_eq!(by_id[0].records().unwrap().len(), 1);

    let whole = session.run("SELECT * FROM users;").unwrap();
    assert_eq!(whole[0].records().unwrap().len(), 2);

    let by_index = session
        .run("SELECT * FROM users WHERE email = 'grace@example.com';")
        .unwrap();
    let found = by_index[0].records().unwrap();
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].0, RecordId::Int(2));
}

#[test]
fn a_filter_over_an_unindexed_field_is_refused_rather_than_scanned() {
    let store = store();
    let mut session = ready(&store);
    session
        .run("DEFINE TABLE users; CREATE users:1 = { name: 'ada' };")
        .unwrap();

    let error = session
        .run("SELECT * FROM users WHERE name = 'ada';")
        .unwrap_err();
    let Error::NoIndexOnField { field, span } = &error else {
        panic!("{error}");
    };
    assert_eq!(field, "name");
    // The message points at the field, not at the statement.
    assert!(span.start > 0, "{error}");
}

#[test]
fn a_create_never_replaces_and_an_update_never_invents() {
    let store = store();
    let mut session = ready(&store);
    session
        .run("DEFINE TABLE users; CREATE users:1 = { name: 'ada' };")
        .unwrap();

    let error = session
        .run("CREATE users:1 = { name: 'grace' };")
        .unwrap_err();
    assert!(matches!(error, Error::RecordExists { .. }), "{error}");

    let error = session
        .run("UPDATE users:9 = { name: 'grace' };")
        .unwrap_err();
    assert!(matches!(error, Error::NoSuchRecord { .. }), "{error}");

    // The refused create left the record alone.
    let outcomes = session.run("SELECT * FROM users:1;").unwrap();
    let records = outcomes[0].records().unwrap();
    assert_eq!(
        field(&records[0].1, "name"),
        &Value::String("ada".to_owned())
    );

    session.run("UPDATE users:1 = { name: 'grace' };").unwrap();
    let outcomes = session.run("SELECT * FROM users:1;").unwrap();
    let records = outcomes[0].records().unwrap();
    assert_eq!(
        field(&records[0].1, "name"),
        &Value::String("grace".to_owned())
    );
}

#[test]
fn a_key_value_space_holds_whole_values_and_replaces_them() {
    let store = store();
    let mut session = ready(&store);
    session.run("DEFINE SPACE sessions;").unwrap();

    session.run("SET sessions:'abc' = 42;").unwrap();
    let outcomes = session.run("GET sessions:'abc';").unwrap();
    assert_eq!(
        outcomes[0].value(),
        Some(&Value::Number(bgv_db_types::Number::Integer(42)))
    );

    // A key-value write replaces; it never merges.
    session.run("SET sessions:'abc' = 'replaced';").unwrap();
    let outcomes = session.run("GET sessions:'abc';").unwrap();
    assert_eq!(
        outcomes[0].value(),
        Some(&Value::String("replaced".to_owned()))
    );

    session.run("DEL sessions:'abc';").unwrap();
    let outcomes = session.run("GET sessions:'abc';").unwrap();
    assert_eq!(outcomes[0].value(), Some(&Value::None));
}

#[test]
fn a_missing_key_and_a_stored_null_are_different_answers() {
    // The whole reason the value system carries both.
    let store = store();
    let mut session = ready(&store);
    session
        .run("DEFINE SPACE sessions; SET sessions:'here' = NULL;")
        .unwrap();

    let stored = session.run("GET sessions:'here';").unwrap();
    assert_eq!(stored[0].value(), Some(&Value::Null));

    let absent = session.run("GET sessions:'gone';").unwrap();
    assert_eq!(absent[0].value(), Some(&Value::None));
}

#[test]
fn keys_lists_a_space_and_a_range_bounds_it() {
    let store = store();
    let mut session = ready(&store);
    session
        .run(
            "DEFINE SPACE sessions;\n\
             SET sessions:'a' = 1;\n\
             SET sessions:'f' = 2;\n\
             SET sessions:'m' = 3;\n\
             SET sessions:'z' = 4;",
        )
        .unwrap();

    let all = session.run("KEYS FROM sessions;").unwrap();
    assert_eq!(all[0].keys().unwrap().len(), 4);

    let bounded = session.run("KEYS FROM sessions RANGE 'a'..'m';").unwrap();
    let keys = bounded[0].keys().unwrap();
    assert_eq!(
        keys,
        [
            RecordId::Text("a".to_owned()),
            RecordId::Text("f".to_owned())
        ]
    );

    let inclusive = session.run("KEYS FROM sessions RANGE 'a'..='m';").unwrap();
    assert_eq!(inclusive[0].keys().unwrap().len(), 3);
}

#[test]
fn a_key_value_read_composes_into_a_record_statement_at_one_snapshot() {
    // §6's claim, made real: the models share a transaction, so they share a
    // snapshot. Two models that cannot are two databases sharing a process.
    let store = store();
    let mut session = ready(&store);
    session
        .run(
            "DEFINE TABLE users;\n\
             DEFINE SPACE emails;\n\
             DEFINE INDEX by_email ON users FIELDS email UNIQUE;\n\
             CREATE users:1 = { name: 'ada', email: 'ada@example.com' };\n\
             SET emails:'primary' = 'ada@example.com';",
        )
        .unwrap();

    let outcomes = session
        .run("SELECT * FROM users WHERE email = GET emails:'primary';")
        .unwrap();
    let records = outcomes[0].records().unwrap();
    assert_eq!(records.len(), 1);
    assert_eq!(
        field(&records[0].1, "name"),
        &Value::String("ada".to_owned())
    );
}

#[test]
fn an_embedded_read_stands_where_a_value_stands() {
    let store = store();
    let mut session = ready(&store);
    session
        .run(
            "DEFINE TABLE users;\n\
             DEFINE TABLE audit;\n\
             DEFINE SPACE sessions;\n\
             CREATE users:1 = { name: 'ada' };\n\
             SET sessions:'abc' = 'a-session';\n\
             CREATE audit:1 = {\n\
               actor:  GET sessions:'abc',\n\
               target: (SELECT * FROM users:1),\n\
             };",
        )
        .unwrap();

    let outcomes = session.run("SELECT * FROM audit:1;").unwrap();
    let record = &outcomes[0].records().unwrap()[0].1;
    assert_eq!(
        field(record, "actor"),
        &Value::String("a-session".to_owned())
    );
    // The embedded read of one record answers with that record's own value.
    assert_eq!(
        field(field(record, "target"), "name"),
        &Value::String("ada".to_owned())
    );
}

#[test]
fn a_unique_index_refuses_a_second_record_with_the_same_value() {
    let store = store();
    let mut session = ready(&store);
    session
        .run(
            "DEFINE TABLE users;\n\
             DEFINE INDEX by_email ON users FIELDS email UNIQUE;\n\
             CREATE users:1 = { email: 'ada@example.com' };",
        )
        .unwrap();

    let error = session
        .run("CREATE users:2 = { email: 'ada@example.com' };")
        .unwrap_err();
    assert!(matches!(error, Error::Store(_)), "{error}");
}

#[test]
fn a_statement_with_no_database_selected_is_an_error_and_not_a_guess() {
    let store = store();
    let mut session = Session::new(&store);
    session
        .run("DEFINE NAMESPACE prod; USE NAMESPACE prod;")
        .unwrap();

    let error = session.run("DEFINE TABLE users;").unwrap_err();
    assert!(matches!(error, Error::NoDatabaseSelected { .. }), "{error}");
}

#[test]
fn a_name_the_catalog_does_not_hold_is_named_back() {
    let store = store();
    let mut session = ready(&store);

    let error = session.run("SELECT * FROM missing;").unwrap_err();
    let Error::Unknown { entity, name, .. } = &error else {
        panic!("{error}");
    };
    assert_eq!(*entity, "table");
    assert_eq!(name, "missing");
}

#[test]
fn if_not_exists_accepts_a_definition_that_already_stands() {
    let store = store();
    let mut session = ready(&store);
    session.run("DEFINE TABLE users;").unwrap();

    let error = session.run("DEFINE TABLE users;").unwrap_err();
    assert!(matches!(error, Error::Store(_)), "{error}");

    session.run("DEFINE TABLE IF NOT EXISTS users;").unwrap();
    session
        .run("DEFINE INDEX IF NOT EXISTS by_email ON users FIELDS email;")
        .unwrap();
    session
        .run("DEFINE INDEX IF NOT EXISTS by_email ON users FIELDS email;")
        .unwrap();
}

#[test]
fn a_session_resolves_a_name_afresh_for_every_statement() {
    // A cached id survives the table it named being dropped and re-created, and
    // reading the wrong table raises nothing anywhere.
    let store = store();
    let mut session = ready(&store);
    session
        .run("DEFINE TABLE users; CREATE users:1 = { name: 'ada' };")
        .unwrap();
    session.run("DROP TABLE users;").unwrap();

    let error = session.run("SELECT * FROM users;").unwrap_err();
    assert!(matches!(&error, Error::Unknown { .. }), "{error}");

    session.run("DEFINE TABLE users;").unwrap();
    let outcomes = session.run("SELECT * FROM users;").unwrap();
    // The re-created table is a different table, and it holds nothing — the
    // records of the old one are not its records.
    assert!(outcomes[0].records().unwrap().is_empty());
}

#[test]
fn a_qualified_name_reaches_another_database_in_the_same_namespace() {
    let store = store();
    let mut session = ready(&store);
    session
        .run(
            "DEFINE DATABASE archive;\n\
             USE DATABASE archive;\n\
             DEFINE TABLE users;\n\
             CREATE users:1 = { name: 'grace' };\n\
             USE DATABASE orders;\n\
             DEFINE TABLE users;\n\
             CREATE users:1 = { name: 'ada' };",
        )
        .unwrap();

    let here = session.run("SELECT * FROM users:1;").unwrap();
    assert_eq!(
        field(&here[0].records().unwrap()[0].1, "name"),
        &Value::String("ada".to_owned())
    );

    let there = session.run("SELECT * FROM archive.users:1;").unwrap();
    assert_eq!(
        field(&there[0].records().unwrap()[0].1, "name"),
        &Value::String("grace".to_owned())
    );
}
