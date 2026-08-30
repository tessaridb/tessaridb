//! Running scripts against a real store.
//!
//! These are the tests the parser's could not be: they assert that a statement
//! reaches the store and that the store answers, which is the only evidence that
//! the language does anything at all.

#![allow(clippy::panic, clippy::unwrap_used, clippy::indexing_slicing)]

use std::sync::Arc;

use tessari_kv::{KvBackend, MemoryBackend};
use tessari_session::{AccessPath, Error, Session};
use tessari_storage::Store;
use tessari_types::{Number, RecordId, Value};

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
        .run("DEFINE TABLE users SCHEMALESS; CREATE users:1 = { name: 'ada' };")
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
         DEFINE COLLECTION accounts;\n\
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
             DEFINE COLLECTION accounts;\n\
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
    session.run("DEFINE TABLE users SCHEMALESS;").unwrap();

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
            "DEFINE TABLE users SCHEMALESS;\n\
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
fn the_access_path_follows_what_exists_rather_than_how_the_query_is_written() {
    use tessari_session::AccessPath;

    let store = store();
    let mut session = ready(&store);
    session
        .run(
            "DEFINE TABLE users SCHEMALESS;\n\
             CREATE users:1 = { name: 'ada lovelace', email: 'ada@example.com' };\n\
             CREATE users:2 = { name: 'grace hopper', email: 'grace@example.com' };",
        )
        .unwrap();

    // No index on `name`, so the same statement reads the table and tests each
    // record. Correct, and it says so.
    let scanned = session
        .run("SELECT * FROM users WHERE name = 'ada lovelace';")
        .unwrap();
    assert_eq!(scanned[0].records().unwrap().len(), 1);
    assert_eq!(scanned[0].path(), Some(AccessPath::Scan));

    // An index exists on `email`, so the equivalent statement becomes an index
    // read. The record is written after the index so that maintenance sees it —
    // see the next test for what happens when it is not.
    session
        .run(
            "DEFINE INDEX by_email ON users FIELDS email UNIQUE;\n\
             CREATE users:3 = { name: 'katherine johnson', email: 'kj@example.com' };",
        )
        .unwrap();
    let indexed = session
        .run("SELECT * FROM users WHERE email = 'kj@example.com';")
        .unwrap();
    assert_eq!(indexed[0].records().unwrap().len(), 1);
    assert_eq!(indexed[0].path(), Some(AccessPath::Index));
}

#[test]
fn an_index_over_rows_that_predate_it_answers_exactly_as_the_scan_did() {
    // The property the whole access-path design rests on: which path runs is
    // decided by what exists, and the answer is the same either way. It only
    // holds because the index is built in the commit that defines it.
    use tessari_session::AccessPath;

    let store = store();
    let mut session = ready(&store);
    session
        .run("DEFINE TABLE users SCHEMALESS; CREATE users:1 = { email: 'ada@example.com' };")
        .unwrap();

    let by_scan = session
        .run("SELECT * FROM users WHERE email = 'ada@example.com';")
        .unwrap();
    assert_eq!(by_scan[0].path(), Some(AccessPath::Scan));

    session
        .run("DEFINE INDEX by_email ON users FIELDS email;")
        .unwrap();

    let by_index = session
        .run("SELECT * FROM users WHERE email = 'ada@example.com';")
        .unwrap();
    assert_eq!(by_index[0].path(), Some(AccessPath::Index));
    assert_eq!(by_index[0].records(), by_scan[0].records());
}

#[test]
fn an_index_defined_inside_an_open_transaction_is_built_with_it() {
    // The case that ruled out calling the standalone backfill from the statement:
    // it drives its own commit loop against the committed tail, so it could
    // neither see this transaction's writes nor land atomically with them.
    use tessari_session::AccessPath;

    let store = store();
    let mut session = ready(&store);
    session.run("DEFINE TABLE users SCHEMALESS;").unwrap();
    session
        .run("CREATE users:1 = { email: 'ada@example.com' };")
        .unwrap();

    session
        .run(
            "BEGIN;\n\
             DEFINE INDEX by_email ON users FIELDS email;\n\
             CREATE users:2 = { email: 'grace@example.com' };\n\
             COMMIT;",
        )
        .unwrap();

    for email in ["ada@example.com", "grace@example.com"] {
        let found = session
            .run(&format!("SELECT * FROM users WHERE email = '{email}';"))
            .unwrap();
        assert_eq!(found[0].path(), Some(AccessPath::Index));
        assert_eq!(found[0].records().unwrap().len(), 1, "{email}");
    }
}

#[test]
fn a_pattern_match_follows_sql_and_is_anchored_to_the_whole_value() {
    let store = store();
    let mut session = ready(&store);
    session
        .run(
            "DEFINE TABLE notes SCHEMALESS;\n\
             CREATE notes:1 = { body: 'Ada Lovelace wrote the first program' };\n\
             CREATE notes:2 = { body: 'Grace Hopper found the first bug' };",
        )
        .unwrap();

    let found = session
        .run("SELECT * FROM notes WHERE body LIKE '%Lovelace%';")
        .unwrap();
    assert_eq!(found[0].records().unwrap().len(), 1);

    // Anchored to the whole value, exactly as SQL is — which is why a substring
    // search needs both wildcards.
    let unanchored = session
        .run("SELECT * FROM notes WHERE body LIKE 'Lovelace';")
        .unwrap();
    assert!(unanchored[0].records().unwrap().is_empty());

    let folded = session
        .run("SELECT * FROM notes WHERE body ILIKE '%lovelace%';")
        .unwrap();
    assert_eq!(folded[0].records().unwrap().len(), 1);

    let none = session
        .run("SELECT * FROM notes WHERE body LIKE '%babbage%';")
        .unwrap();
    assert!(none[0].records().unwrap().is_empty());

    // A field that is not text never matches a text test, rather than erroring:
    // the record simply does not satisfy the filter.
    session.run("CREATE notes:3 = { body: 42 };").unwrap();
    let still = session
        .run("SELECT * FROM notes WHERE body LIKE '%Lovelace%';")
        .unwrap();
    assert_eq!(still[0].records().unwrap().len(), 1);
}

#[test]
fn a_create_never_replaces_and_an_update_never_invents() {
    let store = store();
    let mut session = ready(&store);
    session
        .run("DEFINE TABLE users SCHEMALESS; CREATE users:1 = { name: 'ada' };")
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
        Some(&Value::Number(tessari_types::Number::Integer(42)))
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
fn membership_asks_a_different_question_from_a_pattern() {
    let store = store();
    let mut session = ready(&store);
    session
        .run(
            "DEFINE TABLE notes SCHEMALESS;\n\
             CREATE notes:1 = { body: 'urgent review', tags: ['urgent', 'review'] };",
        )
        .unwrap();

    let member = session
        .run("SELECT * FROM notes WHERE tags CONTAINS 'urgent';")
        .unwrap();
    assert_eq!(member[0].records().unwrap().len(), 1);

    // Membership is exact on the element — not a substring of it.
    let partial = session
        .run("SELECT * FROM notes WHERE tags CONTAINS 'urg';")
        .unwrap();
    assert!(partial[0].records().unwrap().is_empty());

    // And a single value is not a one-element collection, so a query that
    // confuses the two finds nothing rather than looking right.
    let scalar = session
        .run("SELECT * FROM notes WHERE body CONTAINS 'urgent review';")
        .unwrap();
    assert!(scalar[0].records().unwrap().is_empty());

    // The text question is still answered by the text test.
    let text = session
        .run("SELECT * FROM notes WHERE body LIKE '%urgent%';")
        .unwrap();
    assert_eq!(text[0].records().unwrap().len(), 1);
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
            "DEFINE TABLE users SCHEMALESS;\n\
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
            "DEFINE TABLE users SCHEMALESS;\n\
             DEFINE COLLECTION audit;\n\
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
            "DEFINE TABLE users SCHEMALESS;\n\
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

    let error = session.run("DEFINE TABLE users SCHEMALESS;").unwrap_err();
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
    session.run("DEFINE TABLE users SCHEMALESS;").unwrap();

    let error = session.run("DEFINE TABLE users SCHEMALESS;").unwrap_err();
    assert!(matches!(error, Error::Store(_)), "{error}");

    session
        .run("DEFINE TABLE IF NOT EXISTS users SCHEMALESS;")
        .unwrap();
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
        .run("DEFINE TABLE users SCHEMALESS; CREATE users:1 = { name: 'ada' };")
        .unwrap();
    session.run("DROP TABLE users;").unwrap();

    let error = session.run("SELECT * FROM users;").unwrap_err();
    assert!(matches!(&error, Error::Unknown { .. }), "{error}");

    session.run("DEFINE TABLE users SCHEMALESS;").unwrap();
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
             DEFINE TABLE users SCHEMALESS;\n\
             CREATE users:1 = { name: 'grace' };\n\
             USE DATABASE orders;\n\
             DEFINE TABLE users SCHEMALESS;\n\
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

#[test]
fn a_prefix_pattern_on_an_indexed_field_is_a_range_read_answering_exactly_as_the_scan_did() {
    use tessari_session::AccessPath;

    let store = store();
    let mut session = ready(&store);
    session
        .run(
            "DEFINE TABLE people SCHEMALESS;\n\
             CREATE people:1 = { name: 'ada lovelace' };\n\
             CREATE people:2 = { name: 'adam smith' };\n\
             CREATE people:3 = { name: 'grace hopper' };\n\
             CREATE people:4 = { name: 'ad' };",
        )
        .unwrap();

    let by_scan = session
        .run("SELECT * FROM people WHERE name LIKE 'ada%';")
        .unwrap();
    assert_eq!(by_scan[0].path(), Some(AccessPath::Scan));
    assert_eq!(
        by_scan[0].records().unwrap().len(),
        2,
        "'adam smith' begins with 'ada' too"
    );

    session
        .run("DEFINE INDEX by_name ON people FIELDS name;")
        .unwrap();

    let by_index = session
        .run("SELECT * FROM people WHERE name LIKE 'ada%';")
        .unwrap();
    assert_eq!(by_index[0].path(), Some(AccessPath::Index));
    // The records, not the count: a plan that returns the right number of the
    // wrong rows is the failure this comparison exists to catch.
    assert_eq!(by_index[0].records(), by_scan[0].records());

    // A prefix shorter than a stored value, and one that is a whole value.
    for (pattern, expected) in [("ad%", 3), ("ada lovelace%", 1), ("adam%", 1), ("z%", 0)] {
        let found = session
            .run(&format!(
                "SELECT * FROM people WHERE name LIKE '{pattern}';"
            ))
            .unwrap();
        assert_eq!(found[0].path(), Some(AccessPath::Index), "{pattern}");
        assert_eq!(found[0].records().unwrap().len(), expected, "{pattern}");
    }
}

#[test]
fn only_a_trailing_wildcard_uses_the_index_and_the_rest_keep_the_scan() {
    // The invariant that makes this safe to add: where the index cannot answer
    // the exact question, the scan does, and the cost is reported rather than
    // the answer narrowed.
    use tessari_session::AccessPath;

    let store = store();
    let mut session = ready(&store);
    session
        .run(
            "DEFINE TABLE people SCHEMALESS;\n\
             CREATE people:1 = { name: 'ada lovelace' };\n\
             DEFINE INDEX by_name ON people FIELDS name;",
        )
        .unwrap();

    for pattern in ["%lovelace", "%love%", "a_a%", "ada%lace", "%"] {
        let found = session
            .run(&format!(
                "SELECT * FROM people WHERE name LIKE '{pattern}';"
            ))
            .unwrap();
        assert_eq!(found[0].path(), Some(AccessPath::Scan), "{pattern}");
    }

    // ILIKE keeps the scan even in the shape LIKE would index, because the index
    // stores one case and folding at query time is not what it holds.
    let folded = session
        .run("SELECT * FROM people WHERE name ILIKE 'ADA%';")
        .unwrap();
    assert_eq!(folded[0].path(), Some(AccessPath::Scan));
    assert_eq!(folded[0].records().unwrap().len(), 1);
}

#[test]
fn an_escaped_wildcard_in_the_prefix_is_read_as_the_character_it_escapes() {
    use tessari_session::AccessPath;

    let store = store();
    let mut session = ready(&store);
    session
        .run(
            "DEFINE COLLECTION codes;\n\
             CREATE codes:1 = { label: '50% off' };\n\
             CREATE codes:2 = { label: '50 off' };\n\
             DEFINE INDEX by_label ON codes FIELDS label;",
        )
        .unwrap();

    let found = session
        // Two backslashes in the script: the lexer resolves them to one, so the
        // pattern the matcher sees is `50\%%` — an escaped `%`, then a wildcard.
        .run("SELECT * FROM codes WHERE label LIKE '50\\\\%%';")
        .unwrap();
    assert_eq!(found[0].path(), Some(AccessPath::Index));
    assert_eq!(found[0].records().unwrap().len(), 1);
}

#[test]
fn a_prefix_read_sees_this_transactions_own_writes_and_not_its_stale_entries() {
    use tessari_session::AccessPath;

    let store = store();
    let mut session = ready(&store);
    session
        .run(
            "DEFINE TABLE people SCHEMALESS;\n\
             CREATE people:1 = { name: 'ada lovelace' };\n\
             DEFINE INDEX by_name ON people FIELDS name;",
        )
        .unwrap();

    let outcomes = session
        .run(
            "BEGIN;\n\
             CREATE people:2 = { name: 'adam smith' };\n\
             UPDATE people:1 = { name: 'grace hopper' };\n\
             SELECT * FROM people WHERE name LIKE 'ad%';\n\
             COMMIT;",
        )
        .unwrap();

    let found = &outcomes[3];
    assert_eq!(found.path(), Some(AccessPath::Index));
    let records = found.records().unwrap();
    assert_eq!(records.len(), 1, "the new record, and not the moved one");
    assert_eq!(records[0].0.to_string(), "2");
}

#[test]
fn a_declared_type_is_enforced_through_the_language() {
    let store = store();
    let mut session = ready(&store);
    session
        .run(
            "DEFINE TABLE people SCHEMALESS;\n\
             DEFINE FIELD age ON people TYPE int;\n\
             CREATE people:1 = { age: 34 };",
        )
        .unwrap();

    let error = session
        .run("CREATE people:2 = { age: 'thirty four' };")
        .unwrap_err();
    let text = error.to_string();
    assert!(text.contains("age"), "{text}");
    assert!(text.contains("int"), "{text}");
    assert!(text.contains("string"), "{text}");

    // And nothing landed.
    let found = session.run("SELECT * FROM people;").unwrap();
    assert_eq!(found[0].records().unwrap().len(), 1);
}

#[test]
fn a_reserved_word_is_read_as_a_type_name_after_type() {
    // Five of the seventeen spellings are reserved elsewhere in the grammar. If
    // the type position did not read them as text, those types could not be
    // written down at all.
    let store = store();
    let mut session = ready(&store);
    session
        .run(
            "DEFINE TABLE events SCHEMALESS;\n\
             DEFINE FIELD at ON events TYPE datetime;\n\
             DEFINE FIELD who ON events TYPE record;\n\
             DEFINE FIELD span ON events TYPE range;\n\
             DEFINE FIELD tags ON events TYPE set;\n\
             DEFINE FIELD kind ON events TYPE table;",
        )
        .unwrap();

    session
        .run("CREATE events:1 = { at: datetime '2026-08-22T00:00:00Z' };")
        .unwrap();
    assert!(session.run("CREATE events:2 = { at: 7 };").is_err());
}

#[test]
fn a_word_that_is_not_a_type_is_refused_where_a_type_belongs() {
    let store = store();
    let mut session = ready(&store);
    session.run("DEFINE TABLE shapes SCHEMALESS;").unwrap();
    // `geometry` used to stand here, and stopped being a counter-example the
    // moment the value system gained the type. The word chosen now is one no
    // type is ever likely to claim.
    let error = session
        .run("DEFINE FIELD outline ON shapes TYPE parallelogram;")
        .unwrap_err();
    assert!(error.to_string().contains("type name"), "{error}");
}

#[test]
fn the_two_types_the_value_system_gained_are_type_names_the_language_accepts() {
    // The other half of the test above: this is what made `geometry` stop being
    // a word that is not a type, and pinning it here is what stops the pair from
    // drifting apart again.
    let store = store();
    let mut session = ready(&store);
    session.run("DEFINE TABLE shapes SCHEMALESS;").unwrap();
    session
        .run("DEFINE FIELD outline ON shapes TYPE geometry;")
        .expect("geometry is a type");
    session
        .run("DEFINE FIELD pattern ON shapes TYPE regex;")
        .expect("regex is a type");
}

#[test]
fn a_schemafull_table_refuses_a_misspelled_field_and_a_schemaless_one_does_not() {
    let store = store();
    let mut session = ready(&store);
    session
        .run(
            "DEFINE TABLE ledger SCHEMAFULL;\n\
             DEFINE FIELD amount ON ledger TYPE decimal;\n\
             DEFINE TABLE notes SCHEMALESS;\n\
             DEFINE FIELD amount ON notes TYPE decimal;",
        )
        .unwrap();

    let error = session
        .run("CREATE ledger:1 = { amuont: dec 12.34 };")
        .unwrap_err();
    assert!(error.to_string().contains("amuont"), "{error}");

    // The same statement on a schemaless table lands, and the record is one
    // nobody will find by filtering on `amount`.
    session
        .run("CREATE notes:1 = { amuont: dec 12.34 };")
        .unwrap();
}

#[test]
fn a_declaration_and_the_rows_it_constrains_land_together_or_not_at_all() {
    let store = store();
    let mut session = ready(&store);
    session
        .run(
            "DEFINE TABLE guests SCHEMALESS;\n\
             CREATE guests:1 = { handle: 'ada' };",
        )
        .unwrap();

    // Declaring over a row that already violates it writes nothing — not the
    // definition either, so the constraint cannot be believed to hold.
    session.run("CREATE guests:2 = { handle: 7 };").unwrap();
    assert!(
        session
            .run("DEFINE FIELD handle ON guests TYPE string;")
            .is_err()
    );
    // The declaration is gone, so the offending value is still writable.
    session.run("CREATE guests:3 = { handle: 9 };").unwrap();

    session.run("DELETE guests:2;").unwrap();
    session.run("DELETE guests:3;").unwrap();
    session
        .run("DEFINE FIELD handle ON guests TYPE string;")
        .unwrap();
    assert!(session.run("CREATE guests:4 = { handle: 7 };").is_err());
}

#[test]
fn a_row_and_its_declaration_may_be_written_in_either_order_in_one_transaction() {
    let store = store();
    let mut session = ready(&store);
    session.run("DEFINE TABLE staff SCHEMALESS;").unwrap();

    session
        .run(
            "BEGIN;\n\
             CREATE staff:1 = { handle: 'ada' };\n\
             DEFINE FIELD handle ON staff TYPE string;\n\
             COMMIT;",
        )
        .unwrap();

    assert!(
        session
            .run(
                "BEGIN;\n\
                 CREATE staff:2 = { handle: 7 };\n\
                 COMMIT;",
            )
            .is_err()
    );
}

#[test]
fn dropping_a_declaration_leaves_the_data_and_removes_only_the_rule() {
    let store = store();
    let mut session = ready(&store);
    session
        .run(
            "DEFINE TABLE members SCHEMALESS;\n\
             DEFINE FIELD handle ON members TYPE string;\n\
             CREATE members:1 = { handle: 'ada' };",
        )
        .unwrap();
    assert!(session.run("CREATE members:2 = { handle: 7 };").is_err());

    session.run("DROP FIELD handle ON members;").unwrap();
    session.run("CREATE members:2 = { handle: 7 };").unwrap();

    let found = session.run("SELECT * FROM members;").unwrap();
    assert_eq!(found[0].records().unwrap().len(), 2);
}

#[test]
fn a_traversal_is_a_walk_and_says_so() {
    use tessari_session::AccessPath;

    let store = store();
    let mut session = ready(&store);
    session
        .run(
            "DEFINE TABLE users SCHEMALESS;\n\
             DEFINE TABLE follows EDGE;\n\
             CREATE users:1 = { handle: 'ada' };\n\
             CREATE users:2 = { handle: 'grace' };\n\
             RELATE users:1->follows->users:2;",
        )
        .unwrap();

    // `graph` rather than `index`. Every step *is* an index read, but which
    // index is not a choice — an edge table is given one on each endpoint when it
    // is declared — so `index` invited the question of which, and the only answer
    // is the schema. It is also the word `EXPLAIN` has always used for a walk.
    let found = session.run("SELECT * FROM users:1->follows;").unwrap();
    assert_eq!(found[0].path(), Some(AccessPath::Graph));

    let reached = session
        .run("SELECT * FROM users:1->follows->users;")
        .unwrap();
    assert_eq!(reached[0].path(), Some(AccessPath::Graph));
    let records = reached[0].records().unwrap();
    assert_eq!(records.len(), 1);
    assert_eq!(field(&records[0].1, "handle"), &Value::from("grace"));
}

#[test]
fn the_two_directions_answer_the_mirrored_question_over_the_same_data() {
    let store = store();
    let mut session = ready(&store);
    session
        .run(
            "DEFINE TABLE users SCHEMALESS;\n\
             DEFINE TABLE follows EDGE;\n\
             CREATE users:1 = { handle: 'ada' };\n\
             CREATE users:2 = { handle: 'grace' };\n\
             CREATE users:3 = { handle: 'katherine' };\n\
             RELATE users:1->follows->users:3;\n\
             RELATE users:2->follows->users:3;",
        )
        .unwrap();

    // Nobody follows ada; two people follow katherine.
    let out = session
        .run("SELECT * FROM users:1->follows->users;")
        .unwrap();
    assert_eq!(out[0].records().unwrap().len(), 1);
    let into = session
        .run("SELECT * FROM users:3<-follows<-users;")
        .unwrap();
    assert_eq!(into[0].records().unwrap().len(), 2);
    let none = session
        .run("SELECT * FROM users:1<-follows<-users;")
        .unwrap();
    assert!(none[0].records().unwrap().is_empty());
}

#[test]
fn an_edges_own_properties_survive_beside_its_endpoints() {
    let store = store();
    let mut session = ready(&store);
    session
        .run(
            "DEFINE TABLE users SCHEMALESS;\n\
             DEFINE TABLE follows EDGE;\n\
             CREATE users:1 = { handle: 'ada' };\n\
             CREATE users:2 = { handle: 'grace' };\n\
             RELATE users:1->follows->users:2 = { weight: 3 };",
        )
        .unwrap();

    let found = session.run("SELECT * FROM users:1->follows;").unwrap();
    let records = found[0].records().unwrap();
    assert_eq!(records.len(), 1);
    assert_eq!(field(&records[0].1, "weight"), &Value::Number(3.into()));
    // And the endpoints the store wrote, not something the caller had to.
    assert!(matches!(field(&records[0].1, "out"), Value::Record(_)));
    assert!(matches!(field(&records[0].1, "in"), Value::Record(_)));
}

#[test]
fn a_property_that_is_not_a_set_of_named_fields_is_refused_where_it_is_written() {
    let store = store();
    let mut session = ready(&store);
    session
        .run(
            "DEFINE TABLE users SCHEMALESS;\n\
             DEFINE TABLE follows EDGE;\n\
             CREATE users:1 = { handle: 'ada' };\n\
             CREATE users:2 = { handle: 'grace' };",
        )
        .unwrap();
    let error = session
        .run("RELATE users:1->follows->users:2 = 7;")
        .unwrap_err();
    assert!(error.to_string().contains("object"), "{error}");
}

#[test]
fn a_second_arrow_pointing_the_other_way_is_refused_rather_than_answered() {
    // `a->e<-b` would read as "the edges out of a, then whichever record their
    // `out` names" — which is a again, for every edge.
    let store = store();
    let mut session = ready(&store);
    session
        .run("DEFINE TABLE users SCHEMALESS;\nDEFINE TABLE follows EDGE;\nCREATE users:1 = { handle: 'ada' };")
        .unwrap();
    assert!(
        session
            .run("SELECT * FROM users:1->follows<-users;")
            .is_err()
    );
}

#[test]
fn an_edge_table_may_also_be_schemafull_in_either_order() {
    let store = store();
    let mut session = ready(&store);
    session
        .run("DEFINE TABLE knows EDGE SCHEMAFULL;\nDEFINE TABLE likes SCHEMAFULL EDGE;")
        .unwrap();
}

#[test]
fn a_schemafull_edge_table_still_accepts_the_endpoints_the_store_writes() {
    // The two interact: an edge record carries `out` and `in`, which a schemafull
    // table refuses unless they are declared. Declaring them is the edge table's
    // own job, not the caller's — nobody writes those fields by hand.
    let store = store();
    let mut session = ready(&store);
    session
        .run(
            "DEFINE TABLE users SCHEMALESS;\n\
             DEFINE TABLE knows EDGE SCHEMAFULL;\n\
             CREATE users:1 = { handle: 'ada' };\n\
             CREATE users:2 = { handle: 'grace' };",
        )
        .unwrap();
    session.run("RELATE users:1->knows->users:2;").unwrap();
    let found = session.run("SELECT * FROM users:1->knows;").unwrap();
    assert_eq!(found[0].records().unwrap().len(), 1);
}

/// Four people whose `address` differs in shape, so a path reaches a value in
/// some records and nothing at all in others.
fn people_with_addresses(session: &mut Session<'_>) {
    session
        .run(
            "DEFINE COLLECTION people;\n\
             CREATE people:1 = { name: 'ada', address: { city: 'Paris', zip: '75001' }, tags: ['urgent', 'old'] };\n\
             CREATE people:2 = { name: 'grace', address: { city: 'Lyon' }, tags: ['old'] };\n\
             CREATE people:3 = { name: 'alan', address: 'Paris' };\n\
             CREATE people:4 = { name: 'edsger' };",
        )
        .unwrap();
}

#[test]
fn a_filter_reads_a_value_nested_inside_a_record() {
    let store = store();
    let mut session = ready(&store);
    people_with_addresses(&mut session);

    let found = session
        .run("SELECT * FROM people WHERE address.city = 'Paris';")
        .unwrap();
    let records = found[0].records().unwrap();
    assert_eq!(records.len(), 1, "{records:?}");
    assert_eq!(records[0].0, RecordId::Int(1));
}

#[test]
fn a_filter_reads_an_element_of_an_array() {
    let store = store();
    let mut session = ready(&store);
    people_with_addresses(&mut session);

    let first = session
        .run("SELECT * FROM people WHERE tags[0] = 'urgent';")
        .unwrap();
    assert_eq!(first[0].records().unwrap().len(), 1);

    // Position, not membership. `people:2` holds `'old'` too, at position 0.
    let second = session
        .run("SELECT * FROM people WHERE tags[1] = 'old';")
        .unwrap();
    let records = second[0].records().unwrap();
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].0, RecordId::Int(1));
}

#[test]
fn every_way_a_route_ends_early_matches_nothing_rather_than_failing() {
    // A document store whose filter refused documents of a different shape would
    // be refusing the thing it exists to hold. Each of these reaches nothing for
    // a different reason, and all of them answer the same way.
    let store = store();
    let mut session = ready(&store);
    people_with_addresses(&mut session);

    for filter in [
        // No such root.
        "postcode = 'x'",
        // No such field below one that exists.
        "address.street = 'x'",
        // A route through a string: `people:3` holds `address` as text.
        "address.city.first = 'x'",
        // An object addressed by position.
        "address[0] = 'x'",
        // Past the end of an array.
        "tags[9] = 'old'",
        // An array addressed by name.
        "tags.first = 'old'",
    ] {
        let found = session
            .run(&format!("SELECT * FROM people WHERE {filter};"))
            .unwrap();
        assert!(
            found[0].records().unwrap().is_empty(),
            "{filter} found records"
        );
    }
}

#[test]
fn an_index_on_a_path_answers_the_filter_it_projects() {
    use tessari_session::AccessPath;

    let store = store();
    let mut session = ready(&store);
    people_with_addresses(&mut session);

    let scanned = session
        .run("SELECT * FROM people WHERE address.city = 'Paris';")
        .unwrap();
    assert_eq!(scanned[0].path(), Some(AccessPath::Scan));
    let by_scan: Vec<_> = scanned[0].records().unwrap().to_vec();

    // The index is declared over rows that already exist, so it has to build
    // entries for them — including for the two records the path does not reach.
    session
        .run("DEFINE INDEX by_home_city ON people FIELDS address.city;")
        .unwrap();

    let indexed = session
        .run("SELECT * FROM people WHERE address.city = 'Paris';")
        .unwrap();
    assert_eq!(indexed[0].path(), Some(AccessPath::Index));

    // Record for record, not by count: a plan returning the right number of the
    // wrong rows is exactly what a count would miss.
    assert_eq!(indexed[0].records().unwrap(), by_scan.as_slice());
}

#[test]
fn an_index_on_a_path_serves_a_prefix_pattern_as_a_range() {
    use tessari_session::AccessPath;

    let store = store();
    let mut session = ready(&store);
    people_with_addresses(&mut session);
    session
        .run("DEFINE INDEX by_home_city ON people FIELDS address.city;")
        .unwrap();

    // Nothing about the prefix rule depended on the value being top-level: the
    // index stores order-encoded values whatever route produced them.
    let found = session
        .run("SELECT * FROM people WHERE address.city LIKE 'Par%';")
        .unwrap();
    assert_eq!(found[0].path(), Some(AccessPath::Index));
    let records = found[0].records().unwrap();
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].0, RecordId::Int(1));
}

#[test]
fn an_index_answers_the_route_it_projects_and_no_other() {
    use tessari_session::AccessPath;

    let store = store();
    let mut session = ready(&store);
    people_with_addresses(&mut session);
    session
        .run("DEFINE INDEX by_home_city ON people FIELDS address.city;")
        .unwrap();

    // An index on `address.city` is not an index on `address`, for the same
    // reason an index on `(a, b)` is not one on `a`.
    let whole = session
        .run("SELECT * FROM people WHERE address = 'Paris';")
        .unwrap();
    assert_eq!(whole[0].path(), Some(AccessPath::Scan));
    assert_eq!(whole[0].records().unwrap().len(), 1);
    assert_eq!(whole[0].records().unwrap()[0].0, RecordId::Int(3));
}

#[test]
fn a_composite_index_may_mix_a_route_and_a_plain_field() {
    let store = store();
    let mut session = ready(&store);
    people_with_addresses(&mut session);
    session
        .run("DEFINE INDEX by_city_and_name ON people FIELDS address.city, name;")
        .unwrap();

    // A composite serves neither half alone, so this stays a scan — the
    // assertion is that defining it over existing rows succeeded and the read
    // still answers correctly.
    let found = session
        .run("SELECT * FROM people WHERE address.city = 'Lyon';")
        .unwrap();
    let records = found[0].records().unwrap();
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].0, RecordId::Int(2));
}

#[test]
fn a_route_is_not_read_where_a_table_may_stand() {
    // `.` already qualifies a table by its database. If the path rule were read
    // in the `FROM` position, `orders.people` would start meaning "the field
    // `people` inside `orders`" and every qualified read in every script would
    // change meaning at once.
    let store = store();
    let mut session = ready(&store);
    people_with_addresses(&mut session);

    let found = session.run("SELECT * FROM orders.people;").unwrap();
    assert_eq!(found[0].records().unwrap().len(), 4);
}

#[test]
fn a_position_in_a_route_is_a_whole_number() {
    let store = store();
    let mut session = ready(&store);
    people_with_addresses(&mut session);

    for filter in ["tags[-1] = 'old'", "tags['a'] = 'old'", "tags[] = 'old'"] {
        let refused = session.run(&format!("SELECT * FROM people WHERE {filter};"));
        assert!(refused.is_err(), "{filter} was accepted");
    }
}

#[test]
fn a_read_may_ask_for_some_of_a_record_rather_than_all_of_it() {
    let store = store();
    let mut session = ready(&store);
    people_with_addresses(&mut session);

    let found = session.run("SELECT name FROM people:1;").unwrap();
    let records = found[0].records().unwrap();
    assert_eq!(
        records[0].1,
        Value::Object(std::collections::BTreeMap::from([(
            "name".to_owned(),
            Value::String("ada".to_owned())
        )]))
    );
}

#[test]
fn a_projected_route_answers_under_the_last_step_of_its_name() {
    let store = store();
    let mut session = ready(&store);
    people_with_addresses(&mut session);

    let found = session.run("SELECT address.city FROM people:1;").unwrap();
    let record = &found[0].records().unwrap()[0].1;
    assert_eq!(field(record, "city"), &Value::String("Paris".to_owned()));

    let renamed = session
        .run("SELECT address.city AS home, tags[0] AS first_tag FROM people:1;")
        .unwrap();
    let record = &renamed[0].records().unwrap()[0].1;
    assert_eq!(field(record, "home"), &Value::String("Paris".to_owned()));
    assert_eq!(
        field(record, "first_tag"),
        &Value::String("urgent".to_owned())
    );
}

#[test]
fn a_projected_route_that_reaches_nothing_leaves_its_field_out() {
    // Not `none`. `Value::None` says the field is not there, so writing it into
    // an object would say the field is there and holds not-being-there. The
    // consequence is that projected records keep differing shapes, which is the
    // property that lets one table hold documents at all.
    let store = store();
    let mut session = ready(&store);
    people_with_addresses(&mut session);

    let found = session
        .run("SELECT name, address.city FROM people;")
        .unwrap();
    let records = found[0].records().unwrap();
    assert_eq!(records.len(), 4);

    let shapes: Vec<usize> = records
        .iter()
        .map(|(_, record)| match record {
            Value::Object(fields) => fields.len(),
            other => panic!("not an object: {other:?}"),
        })
        .collect();
    // people:1 and :2 have a nested city; :3 holds `address` as text; :4 has none.
    assert_eq!(shapes, vec![2, 2, 1, 1]);
}

#[test]
fn a_projection_applies_to_every_source_and_changes_no_access_path() {
    use tessari_session::AccessPath;

    let store = store();
    let mut session = ready(&store);
    people_with_addresses(&mut session);
    session
        .run(
            "DEFINE INDEX by_home_city ON people FIELDS address.city;\n\
             DEFINE TABLE knows EDGE;\n\
             RELATE people:1->knows->people:2;",
        )
        .unwrap();

    for (script, expected) in [
        ("SELECT name FROM people:1;", AccessPath::Record),
        ("SELECT name FROM people;", AccessPath::Scan),
        (
            "SELECT name FROM people WHERE address.city = 'Paris';",
            AccessPath::Index,
        ),
        (
            "SELECT name FROM people:1->knows->people;",
            AccessPath::Graph,
        ),
    ] {
        let found = session.run(script).unwrap();
        assert_eq!(found[0].path(), Some(expected), "{script}");
        for (_, record) in found[0].records().unwrap() {
            let Value::Object(fields) = record else {
                panic!("{script} did not project an object");
            };
            assert_eq!(fields.keys().collect::<Vec<_>>(), vec!["name"], "{script}");
        }
    }
}

#[test]
fn a_projection_shapes_a_read_standing_in_a_value_position() {
    let store = store();
    let mut session = ready(&store);
    people_with_addresses(&mut session);

    session
        .run("DEFINE COLLECTION audit; CREATE audit:1 = { who: (SELECT name FROM people:1) };")
        .unwrap();
    let found = session.run("SELECT * FROM audit:1;").unwrap();
    let who = field(&found[0].records().unwrap()[0].1, "who");
    assert_eq!(field(who, "name"), &Value::String("ada".to_owned()));
    let Value::Object(fields) = who else {
        panic!("not an object");
    };
    assert_eq!(fields.len(), 1, "the projection did not reach the subquery");
}

/// A table holding several types in one field, so comparison has to say what it
/// means across them rather than only within numbers.
fn mixed_ages(session: &mut Session<'_>) {
    session
        .run(
            "DEFINE COLLECTION people;\n\
             CREATE people:1 = { name: 'ada', age: 17, city: 'Paris' };\n\
             CREATE people:2 = { name: 'grace', age: 45, city: 'Lyon' };\n\
             CREATE people:3 = { name: 'alan', age: 'nineteen', city: 'Paris' };\n\
             CREATE people:4 = { name: 'edsger', age: NULL, city: 'Lyon' };\n\
             CREATE people:5 = { name: 'barbara', city: 'Paris' };",
        )
        .unwrap();
}

fn found_ids(session: &mut Session<'_>, script: &str) -> Vec<RecordId> {
    let outcomes = session.run(script).unwrap();
    outcomes[0]
        .records()
        .unwrap()
        .iter()
        .map(|(id, _)| id.clone())
        .collect()
}

#[test]
fn comparison_follows_the_value_systems_order_and_says_so_across_types() {
    let store = store();
    let mut session = ready(&store);
    mixed_ages(&mut session);

    // 45 only among the numbers — but `'nineteen'` is a string, and a string
    // ranks above a number, so it is above 18 too. Surprising once, and
    // consistent with the order every index range read already uses.
    assert_eq!(
        found_ids(&mut session, "SELECT * FROM people WHERE age > 18;"),
        vec![RecordId::Int(2), RecordId::Int(3)]
    );
    assert_eq!(
        found_ids(&mut session, "SELECT * FROM people WHERE age <= 17;"),
        vec![RecordId::Int(1)]
    );
    // `none` and `null` are not small values. Following the declared order here
    // would put every record with no age recorded under seventeen.
    assert_eq!(
        found_ids(&mut session, "SELECT * FROM people WHERE age < 1000;"),
        vec![RecordId::Int(1), RecordId::Int(2)]
    );
}

#[test]
fn absent_and_null_are_found_by_the_literals_that_name_them() {
    // No `IS NULL` operator, because `= NULL` already means it — and `= NONE`
    // means the other thing, which SQL cannot say at all.
    let store = store();
    let mut session = ready(&store);
    mixed_ages(&mut session);

    assert_eq!(
        found_ids(&mut session, "SELECT * FROM people WHERE age = NONE;"),
        vec![RecordId::Int(5)]
    );
    assert_eq!(
        found_ids(&mut session, "SELECT * FROM people WHERE age = NULL;"),
        vec![RecordId::Int(4)]
    );
}

#[test]
fn conditions_compose_and_parentheses_override_the_precedence() {
    let store = store();
    let mut session = ready(&store);
    mixed_ages(&mut session);

    assert_eq!(
        found_ids(
            &mut session,
            "SELECT * FROM people WHERE city = 'Paris' AND age = 17;"
        ),
        vec![RecordId::Int(1)]
    );
    // `AND` binds tighter than `OR`, so this is `(a AND b) OR c`.
    assert_eq!(
        found_ids(
            &mut session,
            "SELECT * FROM people WHERE city = 'Paris' AND age = 17 OR name = 'grace';"
        ),
        vec![RecordId::Int(1), RecordId::Int(2)]
    );
    // …and parentheses say the other thing.
    assert_eq!(
        found_ids(
            &mut session,
            "SELECT * FROM people WHERE city = 'Paris' AND (age = 17 OR name = 'grace');"
        ),
        vec![RecordId::Int(1)]
    );
    // No three-valued logic: `NOT` over a record with no `age` holds, because
    // the field is absent and absent is not seventeen.
    assert_eq!(
        found_ids(
            &mut session,
            "SELECT * FROM people WHERE NOT (age = 17) AND city = 'Paris';"
        ),
        vec![RecordId::Int(3), RecordId::Int(5)]
    );
}

#[test]
fn membership_reads_from_either_end() {
    let store = store();
    let mut session = ready(&store);
    session
        .run(
            "DEFINE TABLE notes SCHEMALESS;\n\
             CREATE notes:1 = { tags: ['urgent', 'old'] };\n\
             CREATE notes:2 = { tags: ['old'] };",
        )
        .unwrap();

    assert_eq!(
        found_ids(
            &mut session,
            "SELECT * FROM notes WHERE tags CONTAINS 'urgent';"
        ),
        vec![RecordId::Int(1)]
    );
    assert_eq!(
        found_ids(&mut session, "SELECT * FROM notes WHERE 'urgent' IN tags;"),
        vec![RecordId::Int(1)]
    );
}

#[test]
fn a_condition_that_is_not_a_boolean_is_refused_by_the_type_it_found() {
    let store = store();
    let mut session = ready(&store);
    mixed_ages(&mut session);

    let error = session.run("SELECT * FROM people WHERE name;").unwrap_err();
    assert!(
        matches!(
            error,
            Error::ConditionNotBoolean {
                found: "string",
                ..
            }
        ),
        "{error}"
    );
}

#[test]
fn a_bare_name_is_a_route_in_a_condition_and_a_table_in_a_value() {
    let store = store();
    let mut session = ready(&store);
    mixed_ages(&mut session);

    // In a value position `people` is the table itself.
    session
        .run("DEFINE COLLECTION audit; CREATE audit:1 = { subject: people };")
        .unwrap();
    let found = session.run("SELECT * FROM audit:1;").unwrap();
    assert!(matches!(
        field(&found[0].records().unwrap()[0].1, "subject"),
        Value::Table(_)
    ));

    // In a condition the same word reads the record's own field.
    assert_eq!(
        found_ids(&mut session, "SELECT * FROM people WHERE name = 'ada';"),
        vec![RecordId::Int(1)]
    );
}

#[test]
fn an_index_narrows_a_conjunction_and_the_rest_of_it_still_applies() {
    use tessari_session::AccessPath;

    let store = store();
    let mut session = ready(&store);
    mixed_ages(&mut session);

    let script = "SELECT * FROM people WHERE city = 'Paris' AND age = 17;";
    let by_scan = session.run(script).unwrap();
    assert_eq!(by_scan[0].path(), Some(AccessPath::Scan));
    let scanned: Vec<_> = by_scan[0].records().unwrap().to_vec();

    session
        .run("DEFINE INDEX by_city ON people FIELDS city;")
        .unwrap();

    let by_index = session.run(script).unwrap();
    assert_eq!(by_index[0].path(), Some(AccessPath::Index));
    // Record for record. The index answered `city = 'Paris'` — three records —
    // and the other conjunct still had to remove two of them.
    assert_eq!(by_index[0].records().unwrap(), scanned.as_slice());
    assert_eq!(scanned.len(), 1);
}

#[test]
fn neither_side_of_an_or_may_narrow_by_itself() {
    use tessari_session::AccessPath;

    // An index over one half of an `OR` would miss every record satisfying the
    // other half, so the whole condition keeps the scan.
    let store = store();
    let mut session = ready(&store);
    mixed_ages(&mut session);
    session
        .run("DEFINE INDEX by_city ON people FIELDS city;")
        .unwrap();

    let found = session
        .run("SELECT * FROM people WHERE city = 'Paris' OR age = 45;")
        .unwrap();
    assert_eq!(found[0].path(), Some(AccessPath::Scan));
    assert_eq!(found[0].records().unwrap().len(), 4);
}

#[test]
fn a_comparison_against_another_field_is_never_used_as_a_bound() {
    use tessari_session::AccessPath;

    // The right-hand side reads the record, so there is no one value to seek to.
    let store = store();
    let mut session = ready(&store);
    session
        .run(
            "DEFINE COLLECTION pairs;\n\
             DEFINE INDEX by_left ON pairs FIELDS left;\n\
             CREATE pairs:1 = { left: 'a', right: 'a' };\n\
             CREATE pairs:2 = { left: 'a', right: 'b' };",
        )
        .unwrap();

    let found = session
        .run("SELECT * FROM pairs WHERE left = right;")
        .unwrap();
    assert_eq!(found[0].path(), Some(AccessPath::Scan));
    assert_eq!(found[0].records().unwrap().len(), 1);
}

#[test]
fn an_ordered_comparison_is_served_by_the_index_and_answers_what_the_scan_did() {
    use tessari_session::AccessPath;

    // This test used to assert the opposite — that an ordered comparison was
    // *reported* as a scan, because the bounded read was not built and reporting
    // it honestly was the whole of what could be promised. It is built now, so
    // the assertion moves to the thing that actually matters: the rows are the
    // rows the scan gave, and only the path changed.
    let scanned = {
        let store = store();
        let mut session = ready(&store);
        mixed_ages(&mut session);
        let found = session.run("SELECT * FROM people WHERE age > 18;").unwrap();
        assert_eq!(found[0].path(), Some(AccessPath::Scan));
        found[0]
            .records()
            .unwrap()
            .iter()
            .map(|(id, _)| id.clone())
            .collect::<Vec<_>>()
    };

    let store = store();
    let mut session = ready(&store);
    mixed_ages(&mut session);
    session
        .run("DEFINE INDEX by_age ON people FIELDS age;")
        .unwrap();

    let found = session.run("SELECT * FROM people WHERE age > 18;").unwrap();
    assert_eq!(found[0].path(), Some(AccessPath::Index));
    let served: Vec<_> = found[0]
        .records()
        .unwrap()
        .iter()
        .map(|(id, _)| id.clone())
        .collect();
    assert_eq!(served, scanned);
}

#[test]
fn arithmetic_promotes_the_kinds_and_never_truncates_a_division() {
    let store = store();
    let mut session = ready(&store);
    session
        .run(
            "DEFINE COLLECTION lines;\n\
             CREATE lines:1 = { price: dec 2.50, quantity: 3, weight: 1.5 };",
        )
        .unwrap();

    let found = session
        .run(
            "SELECT price * quantity AS total, quantity / 2 AS half, \
             price + weight AS mixed FROM lines:1;",
        )
        .unwrap();
    let record = &found[0].records().unwrap()[0].1;
    // decimal × int is exact; int / int does not truncate; anything with a
    // float is a float and says so.
    assert_eq!(field(record, "total").type_name(), "number");
    // Compared as numbers, not as spellings: a decimal carries a scale, so
    // `1.50` and `1.5` are one value and the store says so.
    assert_eq!(field(record, "total"), &Value::Number(Number::float(7.5)));
    assert_eq!(field(record, "half"), &Value::Number(Number::float(1.5)));
    assert_eq!(field(record, "mixed"), &Value::Number(Number::float(4.0)));
}

#[test]
fn arithmetic_that_has_no_answer_fails_rather_than_producing_one() {
    let store = store();
    let mut session = ready(&store);
    session
        .run("DEFINE COLLECTION lines; CREATE lines:1 = { n: 1, name: 'ada' };")
        .unwrap();

    for script in [
        "SELECT n / 0 AS bad FROM lines:1;",
        "SELECT name + n AS bad FROM lines:1;",
        "SELECT n + 9223372036854775807 + 9223372036854775807 AS bad FROM lines:1;",
    ] {
        assert!(session.run(script).is_err(), "{script} answered");
    }
}

#[test]
fn a_function_computes_over_the_record_it_is_projecting() {
    let store = store();
    let mut session = ready(&store);
    people_with_addresses(&mut session);

    let found = session
        .run(
            "SELECT string::upper(address.city) AS shout, string::len(name) AS letters \
             FROM people:1;",
        )
        .unwrap();
    let record = &found[0].records().unwrap()[0].1;
    assert_eq!(field(record, "shout"), &Value::String("PARIS".to_owned()));
    assert_eq!(field(record, "letters").to_string(), "3");
}

#[test]
fn a_function_also_composes_inside_a_condition() {
    let store = store();
    let mut session = ready(&store);
    people_with_addresses(&mut session);

    assert_eq!(
        found_ids(
            &mut session,
            "SELECT * FROM people WHERE string::len(name) = 3;"
        ),
        vec![RecordId::Int(1)]
    );
    assert_eq!(
        found_ids(
            &mut session,
            "SELECT * FROM people WHERE array::len(tags) > 1;"
        ),
        vec![RecordId::Int(1)]
    );
}

#[test]
fn the_last_element_is_reachable_only_through_a_function() {
    // A path takes a literal position and there is nothing to subtract a length
    // from, which is why `array::last` earns a place under the inclusion rule.
    let store = store();
    let mut session = ready(&store);
    people_with_addresses(&mut session);

    let found = session
        .run("SELECT array::last(tags) AS newest FROM people:1;")
        .unwrap();
    assert_eq!(
        field(&found[0].records().unwrap()[0].1, "newest"),
        &Value::String("old".to_owned())
    );
}

#[test]
fn a_call_is_checked_for_arity_when_the_statement_is_read() {
    let store = store();
    let mut session = ready(&store);
    people_with_addresses(&mut session);

    // Before anything runs, and before any record is in hand.
    assert!(
        session
            .run("SELECT string::len(name, name) AS n FROM people;")
            .is_err()
    );
    assert!(
        session
            .run("SELECT string::nope(name) AS n FROM people;")
            .is_err()
    );
    // A wrong argument type is a different failure, and it names the position.
    let error = session
        .run("SELECT string::len(address) AS n FROM people:1;")
        .unwrap_err();
    let message = error.to_string();
    assert!(message.contains("string::len"), "{message}");
    assert!(message.contains("argument 1"), "{message}");
}

#[test]
fn a_computed_projection_needs_a_name_of_its_own() {
    let store = store();
    let mut session = ready(&store);
    people_with_addresses(&mut session);

    // A bare route names itself; anything computed does not.
    assert!(
        session
            .run("SELECT string::len(name) FROM people;")
            .is_err()
    );
    assert!(session.run("SELECT name FROM people;").is_ok());
}

#[test]
fn a_required_field_must_hold_a_value_and_null_is_not_one() {
    // One marker covering both absence and null, deliberately: a field that must
    // be present but may hold nothing constrains almost nothing.
    let store = store();
    let mut session = ready(&store);
    session
        .run(
            "DEFINE TABLE users SCHEMALESS;\n\
             DEFINE FIELD email ON users TYPE string REQUIRED;",
        )
        .unwrap();

    session
        .run("CREATE users:1 = { email: 'ada@example.com' };")
        .unwrap();
    assert!(session.run("CREATE users:2 = { name: 'grace' };").is_err());
    assert!(session.run("CREATE users:3 = { email: NULL };").is_err());
    assert!(session.run("CREATE users:4 = { email: NONE };").is_err());
}

#[test]
fn requiring_a_field_over_rows_that_lack_it_writes_nothing_at_all() {
    // The symmetry SG4.T2 established: a constraint that can be declared over
    // data violating it is a constraint the store does not have, while every
    // reader afterwards believes it does.
    let store = store();
    let mut session = ready(&store);
    session
        .run("DEFINE TABLE users SCHEMALESS; CREATE users:1 = { name: 'ada' };")
        .unwrap();

    assert!(
        session
            .run("DEFINE FIELD email ON users TYPE string REQUIRED;")
            .is_err()
    );
    // Not even the declaration landed, so a later write is still accepted.
    session.run("CREATE users:2 = { name: 'grace' };").unwrap();
}

#[test]
fn a_default_fills_a_field_a_write_leaves_out() {
    let store = store();
    let mut session = ready(&store);
    session
        .run(
            "DEFINE TABLE notes SCHEMALESS;\n\
             DEFINE FIELD state ON notes TYPE string DEFAULT 'open';\n\
             DEFINE FIELD seen ON notes TYPE int DEFAULT 1 + 1;\n\
             CREATE notes:1 = { body: 'first' };\n\
             CREATE notes:2 = { body: 'second', state: 'closed' };",
        )
        .unwrap();

    let found = session.run("SELECT * FROM notes;").unwrap();
    let records = found[0].records().unwrap();
    assert_eq!(
        field(&records[0].1, "state"),
        &Value::String("open".to_owned())
    );
    assert_eq!(
        field(&records[0].1, "seen"),
        &Value::Number(Number::Integer(2))
    );
    // A supplied value is left alone.
    assert_eq!(
        field(&records[1].1, "state"),
        &Value::String("closed".to_owned())
    );
}

#[test]
fn a_default_is_evaluated_and_not_stored_as_an_expression() {
    let store = store();
    let mut session = ready(&store);
    session
        .run(
            "DEFINE TABLE notes SCHEMALESS;\n\
             DEFINE FIELD created ON notes TYPE datetime DEFAULT time::now();\n\
             CREATE notes:1 = { body: 'first' };",
        )
        .unwrap();

    let found = session.run("SELECT * FROM notes:1;").unwrap();
    let created = field(&found[0].records().unwrap()[0].1, "created");
    assert!(matches!(created, Value::Datetime(_)), "{created:?}");
}

#[test]
fn a_required_field_with_a_default_always_holds_a_value() {
    let store = store();
    let mut session = ready(&store);
    session
        .run(
            "DEFINE TABLE notes SCHEMALESS;\n\
             DEFINE FIELD state ON notes TYPE string REQUIRED DEFAULT 'open';\n\
             CREATE notes:1 = { body: 'first' };",
        )
        .unwrap();

    let found = session.run("SELECT * FROM notes:1;").unwrap();
    assert_eq!(
        field(&found[0].records().unwrap()[0].1, "state"),
        &Value::String("open".to_owned())
    );
}

#[test]
fn a_default_does_not_reach_backwards_over_rows_already_written() {
    // A default is about the moment of writing. A retroactive one would be a
    // bulk rewrite hiding inside a `DEFINE`.
    let store = store();
    let mut session = ready(&store);
    session
        .run("DEFINE TABLE notes SCHEMALESS; CREATE notes:1 = { body: 'first' };")
        .unwrap();
    session
        .run("DEFINE FIELD state ON notes TYPE string DEFAULT 'open';")
        .unwrap();

    let found = session.run("SELECT * FROM notes:1;").unwrap();
    let record = &found[0].records().unwrap()[0].1;
    let Value::Object(fields) = record else {
        panic!("not an object");
    };
    assert!(!fields.contains_key("state"), "{fields:?}");
}

#[test]
fn a_default_is_checked_when_it_is_declared_and_not_when_it_first_bites() {
    let store = store();
    let mut session = ready(&store);
    session.run("DEFINE TABLE notes SCHEMALESS;").unwrap();

    // The wrong type, caught before the declaration lands.
    let error = session
        .run("DEFINE FIELD seen ON notes TYPE int DEFAULT 'open';")
        .unwrap_err();
    assert!(
        matches!(error, Error::DefaultDoesNotMatch { .. }),
        "{error}"
    );

    // A default is a value-position expression, so a bare name is a *table* and
    // not the record's field — and a table that does not exist is refused here
    // rather than on somebody's first write.
    assert!(
        session
            .run("DEFINE FIELD echo ON notes TYPE string DEFAULT body;")
            .is_err()
    );

    // Neither declaration landed, so the field name is still free.
    session
        .run("DEFINE FIELD seen ON notes TYPE int DEFAULT 0;")
        .unwrap();
}

/// A table whose sort key is present, null and absent across its rows, so an
/// order has to say where each goes rather than leaving it to the scan.
fn sortable(session: &mut Session<'_>) {
    session
        .run(
            "DEFINE COLLECTION people;\n\
             CREATE people:1 = { name: 'ada', age: 45, city: 'Paris' };\n\
             CREATE people:2 = { name: 'grace', age: 17, city: 'Lyon' };\n\
             CREATE people:3 = { name: 'alan', age: NULL, city: 'Paris' };\n\
             CREATE people:4 = { name: 'edsger', city: 'Lyon' };\n\
             CREATE people:5 = { name: 'barbara', age: 45, city: 'Paris' };",
        )
        .unwrap();
}

#[test]
fn a_read_may_say_what_order_it_wants() {
    let store = store();
    let mut session = ready(&store);
    sortable(&mut session);

    assert_eq!(
        found_ids(&mut session, "SELECT * FROM people ORDER BY name;"),
        vec![
            RecordId::Int(1), // ada
            RecordId::Int(3), // alan
            RecordId::Int(5), // barbara
            RecordId::Int(4), // edsger
            RecordId::Int(2), // grace
        ]
    );
    assert_eq!(
        found_ids(&mut session, "SELECT * FROM people ORDER BY name DESC;")
            .first()
            .cloned(),
        Some(RecordId::Int(2))
    );
}

#[test]
fn absent_sorts_below_null_sorts_below_every_value() {
    // The opposite of what a comparison does with them, and deliberately: a
    // comparison against a non-value has no answer, while a sort has to put
    // every row somewhere and that somewhere is better stated than left to
    // whichever row the scan reached first.
    let store = store();
    let mut session = ready(&store);
    sortable(&mut session);

    let ordered = found_ids(&mut session, "SELECT * FROM people ORDER BY age;");
    assert_eq!(ordered[0], RecordId::Int(4), "no age at all sorts first");
    assert_eq!(ordered[1], RecordId::Int(3), "then the null");
    assert_eq!(ordered[2], RecordId::Int(2), "then seventeen");
}

#[test]
fn several_keys_sort_by_the_first_then_the_next() {
    let store = store();
    let mut session = ready(&store);
    sortable(&mut session);

    assert_eq!(
        found_ids(
            &mut session,
            "SELECT * FROM people ORDER BY city, name DESC;"
        ),
        vec![
            RecordId::Int(2), // Lyon, grace — reversed within the city
            RecordId::Int(4), // Lyon, edsger
            RecordId::Int(5), // Paris, barbara
            RecordId::Int(3), // Paris, alan
            RecordId::Int(1), // Paris, ada
        ]
    );
}

#[test]
fn equal_keys_are_broken_by_identity_so_the_answer_never_moves() {
    use tessari_session::AccessPath;

    // Without the tiebreak, adding an index would reorder equal rows — an answer
    // that changes when an index appears, which is the shape this store refuses.
    let store = store();
    let mut session = ready(&store);
    sortable(&mut session);

    let script = "SELECT * FROM people WHERE city = 'Paris' ORDER BY age;";
    let by_scan = session.run(script).unwrap();
    assert_eq!(by_scan[0].path(), Some(AccessPath::Scan));
    let scanned: Vec<_> = by_scan[0].records().unwrap().to_vec();

    session
        .run("DEFINE INDEX by_city ON people FIELDS city;")
        .unwrap();
    let by_index = session.run(script).unwrap();
    assert_eq!(by_index[0].path(), Some(AccessPath::Index));
    assert_eq!(by_index[0].records().unwrap(), scanned.as_slice());
    // people:1 and people:5 both hold 45, and identity decides.
    assert_eq!(scanned[1].0, RecordId::Int(1));
    assert_eq!(scanned[2].0, RecordId::Int(5));
}

#[test]
fn a_bound_takes_a_window_after_the_order_and_not_before() {
    let store = store();
    let mut session = ready(&store);
    sortable(&mut session);

    assert_eq!(
        found_ids(&mut session, "SELECT * FROM people ORDER BY name LIMIT 2;"),
        vec![RecordId::Int(1), RecordId::Int(3)]
    );
    assert_eq!(
        found_ids(
            &mut session,
            "SELECT * FROM people ORDER BY name START 2 LIMIT 2;"
        ),
        vec![RecordId::Int(5), RecordId::Int(4)]
    );
    // Past the end is a state, not a mistake.
    assert!(found_ids(&mut session, "SELECT * FROM people ORDER BY name START 99;").is_empty());
    // And a bound with no order still applies, over the order the store has.
    assert_eq!(
        found_ids(&mut session, "SELECT * FROM people LIMIT 1;").len(),
        1
    );
}

#[test]
fn a_sort_key_may_be_a_route_or_a_projected_name() {
    let store = store();
    let mut session = ready(&store);
    people_with_addresses(&mut session);

    // A route into the record. `people:3` holds `address` as text, so the route
    // reaches nothing and it sorts with the ones that have no address at all —
    // last, under `DESC`.
    let by_route = session
        .run("SELECT * FROM people ORDER BY address.city DESC;")
        .unwrap();
    let routed: Vec<_> = by_route[0]
        .records()
        .unwrap()
        .iter()
        .map(|(id, _)| id.clone())
        .collect();
    assert_eq!(routed[0], RecordId::Int(1), "Paris is the highest city");
    assert_eq!(routed[1], RecordId::Int(2), "then Lyon");

    // …and the name the projection gave it answers the same way, which is the
    // point: one order, whichever way the key is named.
    let by_name = session
        .run("SELECT address.city AS home FROM people ORDER BY home DESC;")
        .unwrap();
    let named: Vec<_> = by_name[0]
        .records()
        .unwrap()
        .iter()
        .map(|(id, _)| id.clone())
        .collect();
    assert_eq!(named, routed);
}

#[test]
fn the_words_that_shape_a_read_are_not_reserved_names() {
    // `ORDER`, `BY`, `LIMIT` and `START` are contextual: reserving them would
    // take four perfectly good names away from data that already exists, and
    // this language has a rule about that.
    let store = store();
    let mut session = ready(&store);
    session
        .run(
            "DEFINE COLLECTION order;\n\
             CREATE order:1 = { limit: 10, by: 'ada', start: 1 };",
        )
        .unwrap();

    assert_eq!(
        found_ids(&mut session, "SELECT * FROM order WHERE limit = 10;"),
        vec![RecordId::Int(1)]
    );
    assert_eq!(
        found_ids(&mut session, "SELECT * FROM order ORDER BY by LIMIT 1;"),
        vec![RecordId::Int(1)]
    );
}

#[test]
fn a_fold_answers_once_for_many_records() {
    let store = store();
    let mut session = ready(&store);
    sortable(&mut session);

    let found = session.run("SELECT count(*) AS n FROM people;").unwrap();
    let records = found[0].records().unwrap();
    assert_eq!(records.len(), 1, "one answer, not one per record");
    assert_eq!(
        field(&records[0].1, "n"),
        &Value::Number(Number::Integer(5))
    );
}

#[test]
fn counting_a_value_and_counting_records_are_different_questions() {
    let store = store();
    let mut session = ready(&store);
    sortable(&mut session);

    // people:4 has no age at all and people:3 holds null; neither is a value.
    let found = session
        .run("SELECT count(*) AS records, count(age) AS ages FROM people;")
        .unwrap();
    let record = &found[0].records().unwrap()[0].1;
    assert_eq!(field(record, "records"), &Value::Number(Number::Integer(5)));
    assert_eq!(field(record, "ages"), &Value::Number(Number::Integer(3)));
}

#[test]
fn grouping_answers_once_per_distinct_key() {
    let store = store();
    let mut session = ready(&store);
    sortable(&mut session);

    let found = session
        .run("SELECT city, count(*) AS n FROM people GROUP BY city;")
        .unwrap();
    let records = found[0].records().unwrap();
    assert_eq!(records.len(), 2);
    // Groups come out in the value system's order, so Lyon precedes Paris.
    assert_eq!(field(&records[0].1, "city"), &Value::from("Lyon"));
    assert_eq!(
        field(&records[0].1, "n"),
        &Value::Number(Number::Integer(2))
    );
    assert_eq!(field(&records[1].1, "city"), &Value::from("Paris"));
    assert_eq!(
        field(&records[1].1, "n"),
        &Value::Number(Number::Integer(3))
    );
}

#[test]
fn the_folds_ignore_what_holds_nothing_and_say_so_when_there_is_nothing() {
    let store = store();
    let mut session = ready(&store);
    sortable(&mut session);

    let found = session
        .run(
            "SELECT sum(age) AS total, mean(age) AS average, \
             min(age) AS youngest, max(age) AS oldest FROM people;",
        )
        .unwrap();
    let record = &found[0].records().unwrap()[0].1;
    // 45 + 17 + 45; the null and the absent are not values.
    assert_eq!(field(record, "total"), &Value::Number(Number::Integer(107)));
    assert_eq!(
        field(record, "youngest"),
        &Value::Number(Number::Integer(17))
    );
    assert_eq!(field(record, "oldest"), &Value::Number(Number::Integer(45)));
    // A mean is exact where the arithmetic allows: 107/3 answers as a decimal
    // carrying the division's full precision, not as a float that has already
    // rounded. Asserted over data whose average is whole, so the assertion is
    // about the value rather than about how many digits survived.
    session
        .run(
            "DEFINE COLLECTION scores;\n\
             CREATE scores:1 = { n: 10 };\n\
             CREATE scores:2 = { n: 20 };\n\
             CREATE scores:3 = { n: NULL };",
        )
        .unwrap();
    let exact = session
        .run("SELECT mean(n) AS average FROM scores;")
        .unwrap();
    assert_eq!(
        field(&exact[0].records().unwrap()[0].1, "average"),
        &Value::Number(Number::Integer(15)),
        "a mean compares by value, whatever numeric kind carries it"
    );

    // Over a group holding no numbers at all: sum is zero, mean is nothing.
    session
        .run("DEFINE COLLECTION empty; CREATE empty:1 = { name: 'ada' };")
        .unwrap();
    let none = session
        .run("SELECT sum(age) AS total, mean(age) AS average FROM empty;")
        .unwrap();
    let record = &none[0].records().unwrap()[0].1;
    assert_eq!(field(record, "total"), &Value::Number(Number::Integer(0)));
    let Value::Object(fields) = record else {
        panic!("not an object");
    };
    assert!(
        !fields.contains_key("average"),
        "a mean of nothing is nothing, and nothing is left out"
    );
}

#[test]
fn a_fold_over_something_that_is_not_a_number_fails_rather_than_skipping() {
    // A silent skip would make a wrong total look like a right one.
    let store = store();
    let mut session = ready(&store);
    sortable(&mut session);

    let error = session
        .run("SELECT sum(name) AS total FROM people;")
        .unwrap_err();
    assert!(matches!(error, Error::NotSummable { .. }), "{error}");
}

#[test]
fn a_grouped_read_may_answer_only_with_its_keys_and_its_folds() {
    let store = store();
    let mut session = ready(&store);
    sortable(&mut session);

    // `name` has as many values as the group has records, and picking one
    // silently is how a wrong number reaches a report.
    assert!(
        session
            .run("SELECT name, count(*) AS n FROM people GROUP BY city;")
            .is_err()
    );
    // `*` over a group would answer with whichever record came last.
    assert!(session.run("SELECT * FROM people GROUP BY city;").is_err());
    // And a fold has no name of its own.
    assert!(session.run("SELECT count(*) FROM people;").is_err());
    // `*` means the records themselves, which only `count` can fold.
    assert!(session.run("SELECT sum(*) AS n FROM people;").is_err());
}

#[test]
fn ordering_and_bounding_a_grouped_read_shapes_the_groups() {
    let store = store();
    let mut session = ready(&store);
    sortable(&mut session);

    let found = session
        .run(
            "SELECT city, count(*) AS n FROM people \
             GROUP BY city ORDER BY n DESC LIMIT 1;",
        )
        .unwrap();
    let records = found[0].records().unwrap();
    assert_eq!(records.len(), 1);
    assert_eq!(field(&records[0].1, "city"), &Value::from("Paris"));
}

#[test]
fn grouping_by_several_keys_groups_by_the_combination() {
    let store = store();
    let mut session = ready(&store);
    session
        .run(
            "DEFINE COLLECTION sales;\n\
             CREATE sales:1 = { city: 'Paris', year: 2025, n: 1 };\n\
             CREATE sales:2 = { city: 'Paris', year: 2026, n: 2 };\n\
             CREATE sales:3 = { city: 'Paris', year: 2026, n: 4 };\n\
             CREATE sales:4 = { city: 'Lyon', year: 2026, n: 8 };",
        )
        .unwrap();

    let found = session
        .run("SELECT city, year, sum(n) AS total FROM sales GROUP BY city, year;")
        .unwrap();
    let records = found[0].records().unwrap();
    assert_eq!(records.len(), 3);
    assert_eq!(
        field(&records[2].1, "total"),
        &Value::Number(Number::Integer(6)),
        "Paris in 2026 is two rows folded into one"
    );
}

#[test]
fn a_field_called_count_is_still_a_field() {
    // Only `count(` is a fold; a bare `count` is a route into the record, the
    // same rule the words that shape a read already follow.
    let store = store();
    let mut session = ready(&store);
    session
        .run("DEFINE COLLECTION tallies; CREATE tallies:1 = { count: 7 };")
        .unwrap();

    assert_eq!(
        found_ids(&mut session, "SELECT * FROM tallies WHERE count = 7;"),
        vec![RecordId::Int(1)]
    );
    let found = session.run("SELECT count FROM tallies;").unwrap();
    assert_eq!(
        field(&found[0].records().unwrap()[0].1, "count"),
        &Value::Number(Number::Integer(7))
    );
}

/// Notes carrying text an analyzer has an opinion about, and text it does not.
fn searchable(session: &mut Session<'_>) {
    session
        .run(
            "DEFINE ANALYZER simple FILTERS lowercase, ascii;\n\
             DEFINE TABLE notes SCHEMALESS;\n\
             DEFINE FIELD body ON notes TYPE string ANALYZER simple;\n\
             DEFINE FIELD note ON notes TYPE any ANALYZER simple;\n\
             CREATE notes:1 = { body: 'Ada Lovelace wrote the first program', title: 'Ada' };\n\
             CREATE notes:2 = { body: 'A note about the café on the corner' };\n\
             CREATE notes:3 = { body: 'lovelacex is not the same word' };\n\
             CREATE notes:4 = { title: 'no body at all' };\n\
             CREATE notes:5 = { note: 42 };",
        )
        .unwrap();
}

#[test]
fn a_term_is_a_whole_word_which_is_what_makes_it_not_a_pattern() {
    let store = store();
    let mut session = ready(&store);
    searchable(&mut session);

    assert_eq!(
        found_ids(
            &mut session,
            "SELECT * FROM notes WHERE body MATCHES 'lovelace';"
        ),
        vec![RecordId::Int(1)]
    );
    // The same question asked of characters finds both — `lovelacex` holds
    // those letters and is not that word. That difference is why `MATCHES` is
    // its own operator rather than a spelling of `LIKE`. (And the pattern is
    // written lower-case-free because `LIKE` matches characters exactly, where
    // the analyzer has already folded case for `MATCHES` — a second difference
    // in the same line.)
    assert_eq!(
        found_ids(
            &mut session,
            "SELECT * FROM notes WHERE body LIKE '%ovelace%';"
        ),
        vec![RecordId::Int(1), RecordId::Int(3)]
    );
}

#[test]
fn the_filters_are_what_make_two_spellings_one_term() {
    let store = store();
    let mut session = ready(&store);
    searchable(&mut session);

    assert_eq!(
        found_ids(
            &mut session,
            "SELECT * FROM notes WHERE body MATCHES 'cafe';"
        ),
        vec![RecordId::Int(2)]
    );
    assert_eq!(
        found_ids(
            &mut session,
            "SELECT * FROM notes WHERE body MATCHES 'LOVELACE';"
        ),
        vec![RecordId::Int(1)]
    );
}

#[test]
fn several_terms_mean_all_of_them() {
    let store = store();
    let mut session = ready(&store);
    searchable(&mut session);

    assert_eq!(
        found_ids(
            &mut session,
            "SELECT * FROM notes WHERE body MATCHES 'ada program';"
        ),
        vec![RecordId::Int(1)]
    );
    assert!(
        found_ids(
            &mut session,
            "SELECT * FROM notes WHERE body MATCHES 'ada babbage';"
        )
        .is_empty()
    );
}

#[test]
fn a_field_with_no_analyzer_holds_no_terms_rather_than_failing() {
    // A schemaless table is allowed to hold text nobody has declared anything
    // about, so refusing the query would make that a mistake.
    let store = store();
    let mut session = ready(&store);
    searchable(&mut session);

    assert!(
        found_ids(
            &mut session,
            "SELECT * FROM notes WHERE title MATCHES 'ada';"
        )
        .is_empty()
    );
    // …and neither does a value that is not text, even on a field that does
    // declare an analyzer: there is nothing to tokenise.
    assert!(found_ids(&mut session, "SELECT * FROM notes WHERE note MATCHES '42';").is_empty());
}

#[test]
fn an_analyzer_is_declared_once_and_attached_where_it_is_needed() {
    let store = store();
    let mut session = ready(&store);
    searchable(&mut session);

    // One declaration, two fields on two tables.
    session
        .run(
            "DEFINE TABLE letters SCHEMALESS;\n\
             DEFINE FIELD text ON letters TYPE string ANALYZER simple;\n\
             CREATE letters:1 = { text: 'Dear Ada' };",
        )
        .unwrap();
    assert_eq!(
        found_ids(
            &mut session,
            "SELECT * FROM letters WHERE text MATCHES 'ada';"
        ),
        vec![RecordId::Int(1)]
    );

    // The name is unique, like every other name.
    assert!(
        session
            .run("DEFINE ANALYZER simple FILTERS lowercase;")
            .is_err()
    );
    session
        .run("DEFINE ANALYZER IF NOT EXISTS simple FILTERS lowercase;")
        .unwrap();
}

#[test]
fn a_search_composes_with_everything_else_a_condition_can_say() {
    let store = store();
    let mut session = ready(&store);
    searchable(&mut session);

    assert_eq!(
        found_ids(
            &mut session,
            "SELECT * FROM notes WHERE body MATCHES 'lovelace' AND title = 'Ada';"
        ),
        vec![RecordId::Int(1)]
    );
    assert_eq!(
        found_ids(
            &mut session,
            "SELECT * FROM notes WHERE NOT (body MATCHES 'lovelace') AND title = 'Ada';"
        ),
        Vec::new()
    );
}

#[test]
fn a_declared_analyzer_survives_a_replica_replaying_the_log() {
    use tessari_storage::Store;

    let store = store();
    let mut session = ready(&store);
    searchable(&mut session);

    let replica_backend =
        std::sync::Arc::new(tessari_kv::MemoryBackend::new()) as std::sync::Arc<dyn KvBackend>;
    let replica = Store::open(std::sync::Arc::clone(&replica_backend)).unwrap();
    for (sequence, record) in store
        .log_records(tessari_types::Sequence::ZERO, 1024)
        .unwrap()
    {
        replica.apply_record(sequence, &record).unwrap();
    }

    // The analyzer, the attachment and the records all arrived through one log,
    // so the replica answers the same search.
    let mut mirrored = Session::new(&replica);
    mirrored.run("USE NAMESPACE prod DATABASE orders;").unwrap();
    let found = mirrored
        .run("SELECT * FROM notes WHERE body MATCHES 'lovelace';")
        .unwrap();
    assert_eq!(found[0].records().unwrap().len(), 1);
}

#[test]
fn a_search_index_changes_the_cost_and_not_the_answer() {
    use tessari_session::AccessPath;

    // The claim SGC.T1's design exists to make true, asserted the way this store
    // asserts every index: record for record against the same query before the
    // index existed. A plan returning the right *number* of the wrong rows is
    // what a count would miss.
    let store = store();
    let mut session = ready(&store);
    searchable(&mut session);

    let script = "SELECT * FROM notes WHERE body MATCHES 'lovelace';";
    let by_scan = session.run(script).unwrap();
    assert_eq!(by_scan[0].path(), Some(AccessPath::Scan));
    let scanned: Vec<_> = by_scan[0].records().unwrap().to_vec();

    session
        .run("DEFINE INDEX by_body ON notes FIELDS body SEARCH;")
        .unwrap();

    let by_index = session.run(script).unwrap();
    assert_eq!(by_index[0].path(), Some(AccessPath::Index));
    assert_eq!(by_index[0].records().unwrap(), scanned.as_slice());
    assert_eq!(scanned.len(), 1);
}

#[test]
fn a_search_index_built_over_existing_rows_answers_every_shape_of_query() {
    let store = store();
    let mut session = ready(&store);
    searchable(&mut session);
    session
        .run("DEFINE INDEX by_body ON notes FIELDS body SEARCH;")
        .unwrap();

    // Several terms: the postings of each, intersected.
    assert_eq!(
        found_ids(
            &mut session,
            "SELECT * FROM notes WHERE body MATCHES 'ada program';"
        ),
        vec![RecordId::Int(1)]
    );
    // A term nothing holds.
    assert!(
        found_ids(
            &mut session,
            "SELECT * FROM notes WHERE body MATCHES 'babbage';"
        )
        .is_empty()
    );
    // The filters still apply, because they are the field's and not the index's.
    assert_eq!(
        found_ids(
            &mut session,
            "SELECT * FROM notes WHERE body MATCHES 'CAFE';"
        ),
        vec![RecordId::Int(2)]
    );
}

#[test]
fn an_update_takes_back_the_postings_the_record_no_longer_earns() {
    let store = store();
    let mut session = ready(&store);
    searchable(&mut session);
    session
        .run("DEFINE INDEX by_body ON notes FIELDS body SEARCH;")
        .unwrap();

    assert_eq!(
        found_ids(
            &mut session,
            "SELECT * FROM notes WHERE body MATCHES 'lovelace';"
        )
        .len(),
        1
    );
    session
        .run("UPDATE notes:1 = { body: 'Grace Hopper wrote a compiler' };")
        .unwrap();

    // The old term is gone and the new one is there — an orphan posting would
    // show up here as a record that no longer matches.
    assert!(
        found_ids(
            &mut session,
            "SELECT * FROM notes WHERE body MATCHES 'lovelace';"
        )
        .is_empty()
    );
    assert_eq!(
        found_ids(
            &mut session,
            "SELECT * FROM notes WHERE body MATCHES 'hopper';"
        ),
        vec![RecordId::Int(1)]
    );

    // And a delete takes the rest.
    session.run("DELETE notes:1;").unwrap();
    assert!(
        found_ids(
            &mut session,
            "SELECT * FROM notes WHERE body MATCHES 'hopper';"
        )
        .is_empty()
    );
}

#[test]
fn an_ordered_index_is_never_asked_a_term_question() {
    use tessari_session::AccessPath;

    // A search index cannot answer an equality and an ordered index cannot
    // answer a term. Asking the wrong one would return the wrong rows rather
    // than none, so the shape is checked against the index.
    let store = store();
    let mut session = ready(&store);
    searchable(&mut session);
    session
        .run("DEFINE INDEX by_body_value ON notes FIELDS body;")
        .unwrap();

    let found = session
        .run("SELECT * FROM notes WHERE body MATCHES 'lovelace';")
        .unwrap();
    assert_eq!(found[0].path(), Some(AccessPath::Scan));
    assert_eq!(found[0].records().unwrap().len(), 1);
}

#[test]
fn a_replica_builds_the_same_postings_from_the_same_log() {
    use tessari_storage::Store;

    let store = store();
    let mut session = ready(&store);
    searchable(&mut session);
    session
        .run("DEFINE INDEX by_body ON notes FIELDS body SEARCH;")
        .unwrap();

    let replica_backend =
        std::sync::Arc::new(tessari_kv::MemoryBackend::new()) as std::sync::Arc<dyn KvBackend>;
    let replica = Store::open(std::sync::Arc::clone(&replica_backend)).unwrap();
    for (sequence, record) in store
        .log_records(tessari_types::Sequence::ZERO, 4096)
        .unwrap()
    {
        replica.apply_record(sequence, &record).unwrap();
    }

    let mut mirrored = Session::new(&replica);
    mirrored.run("USE NAMESPACE prod DATABASE orders;").unwrap();
    let found = mirrored
        .run("SELECT * FROM notes WHERE body MATCHES 'lovelace';")
        .unwrap();
    assert_eq!(found[0].path(), Some(tessari_session::AccessPath::Index));
    assert_eq!(found[0].records().unwrap().len(), 1);
}

/// Notes with embeddings, one missing and one the wrong shape.
fn embedded(session: &mut Session<'_>) {
    session
        .run(
            "DEFINE COLLECTION notes;\n\
             CREATE notes:1 = { title: 'east', embedding: [1.0, 0.0] };\n\
             CREATE notes:2 = { title: 'north-east', embedding: [0.7, 0.7] };\n\
             CREATE notes:3 = { title: 'north', embedding: [0.0, 1.0] };\n\
             CREATE notes:4 = { title: 'west', embedding: [-1.0, 0.0] };\n\
             CREATE notes:5 = { title: 'wrong shape', embedding: [1.0] };\n\
             CREATE notes:6 = { title: 'none at all' };",
        )
        .unwrap();
}

#[test]
fn a_nearest_neighbour_query_is_an_order_and_a_bound() {
    // No operator of its own: "the three most similar" is something this
    // language could already say, once it could say the distance.
    let store = store();
    let mut session = ready(&store);
    embedded(&mut session);

    assert_eq!(
        found_ids(
            &mut session,
            "SELECT * FROM notes ORDER BY vector::cosine(embedding, [1.0, 0.0]) LIMIT 3;"
        ),
        vec![RecordId::Int(1), RecordId::Int(2), RecordId::Int(3)]
    );
}

#[test]
fn a_record_with_no_distance_is_infinitely_far_rather_than_nearest() {
    let store = store();
    let mut session = ready(&store);
    embedded(&mut session);

    // Six records answer, not four: the wrong-shaped and the missing embedding
    // are infinitely far rather than absent, so they sort **last** and a bounded
    // read never mistakes them for neighbours.
    let ordered = found_ids(
        &mut session,
        "SELECT * FROM notes ORDER BY vector::cosine(embedding, [1.0, 0.0]);",
    );
    assert_eq!(ordered.len(), 6);
    assert_eq!(ordered[0], RecordId::Int(1), "the nearest first");
    assert_eq!(ordered[3], RecordId::Int(4), "then the furthest real one");
    assert_eq!(
        &ordered[4..],
        &[RecordId::Int(5), RecordId::Int(6)],
        "and what has no distance is last"
    );
}

#[test]
fn the_three_distances_answer_three_questions() {
    let store = store();
    let mut session = ready(&store);
    embedded(&mut session);

    let found = session
        .run(
            "SELECT vector::cosine(embedding, [2.0, 0.0]) AS angle, \
             vector::euclidean(embedding, [2.0, 0.0]) AS gap, \
             vector::dot(embedding, [2.0, 0.0]) AS inner \
             FROM notes:1;",
        )
        .unwrap();
    let record = &found[0].records().unwrap()[0].1;
    // Cosine ignores magnitude; the other two do not. That is why all three
    // exist rather than one being picked.
    assert_eq!(field(record, "angle"), &Value::Number(Number::float(0.0)));
    assert_eq!(field(record, "gap"), &Value::Number(Number::float(1.0)));
    assert_eq!(field(record, "inner"), &Value::Number(Number::float(2.0)));
}

#[test]
fn ordering_by_an_expression_works_for_anything_computed() {
    // The widening a nearest-neighbour query needed turns out to be general.
    let store = store();
    let mut session = ready(&store);
    session
        .run(
            "DEFINE COLLECTION people;\n\
             CREATE people:1 = { name: 'barbara' };\n\
             CREATE people:2 = { name: 'ada' };\n\
             CREATE people:3 = { name: 'grace' };",
        )
        .unwrap();

    assert_eq!(
        found_ids(
            &mut session,
            "SELECT * FROM people ORDER BY string::len(name), name;"
        ),
        vec![RecordId::Int(2), RecordId::Int(3), RecordId::Int(1)]
    );
    // And `DESC` still reverses it.
    assert_eq!(
        found_ids(
            &mut session,
            "SELECT * FROM people ORDER BY string::len(name) DESC LIMIT 1;"
        ),
        vec![RecordId::Int(1)]
    );
}

// ------------------------------------------------------------------ identity

/// A store with one user of each role, and a table to try them on.
///
/// The bootstrap is the real one: the **first** user is declared while the store
/// is still open, and every user after it is declared by signing in as that
/// first one. There is no other way in, which is the point of the rule.
fn guarded(session: &mut Session<'_>) {
    session
        .run(
            "DEFINE COLLECTION notes;\n\
             CREATE notes:1 = { body: 'written while open' };\n\
             DEFINE USER root ROLE owner PASSWORD 'root secret';",
        )
        .unwrap();
    session.sign_in("root", "root secret").unwrap();
    session
        .run(
            "DEFINE USER ada ON prod.orders ROLE editor PASSWORD 'correct horse';\n\
             DEFINE USER grace ON prod.orders ROLE viewer PASSWORD 'watch only';",
        )
        .unwrap();
    session.sign_out();
}

#[test]
fn a_store_with_no_users_is_open_and_the_first_one_closes_it() {
    // Requiring a signin against an empty store locks everybody out of it with
    // no way in to fix that.
    let store = store();
    let mut session = ready(&store);
    session
        .run("DEFINE COLLECTION notes; CREATE notes:1 = { body: 'anyone' };")
        .unwrap();

    session
        .run("DEFINE USER root ROLE owner PASSWORD 'root secret';")
        .unwrap();

    let error = session.run("SELECT * FROM notes;").unwrap_err();
    assert!(matches!(error, Error::NotSignedIn { .. }), "{error}");
}

#[test]
fn a_signin_matches_the_password_and_says_nothing_about_which_half_was_wrong() {
    let store = store();
    let mut session = ready(&store);
    guarded(&mut session);

    // One message for a wrong name and a wrong password alike: telling them
    // apart tells an attacker which half to keep guessing at.
    let wrong_password = session.sign_in("ada", "incorrect horse").unwrap_err();
    let wrong_name = session.sign_in("nobody", "correct horse").unwrap_err();
    assert_eq!(wrong_password.to_string(), wrong_name.to_string());
    assert!(matches!(wrong_password, Error::SignInRefused));

    session.sign_in("ada", "correct horse").unwrap();
    session.run("SELECT * FROM notes;").unwrap();
}

#[test]
fn a_role_decides_what_a_signed_in_session_may_do() {
    let store = store();
    let mut session = ready(&store);
    guarded(&mut session);

    // A viewer reads and does nothing else.
    session.sign_in("grace", "watch only").unwrap();
    session.run("SELECT * FROM notes;").unwrap();
    for refused in [
        "CREATE notes:2 = { body: 'no' };",
        "UPDATE notes:1 = { body: 'no' };",
        "DELETE notes:1;",
        "DEFINE COLLECTION more;",
        "DROP TABLE notes;",
    ] {
        let error = session.run(refused).unwrap_err();
        assert!(
            matches!(error, Error::RoleForbids { role: "viewer", .. }),
            "{refused} gave {error}"
        );
    }

    // An editor writes and defines structure, and may not declare users.
    session.sign_in("ada", "correct horse").unwrap();
    session.run("CREATE notes:2 = { body: 'yes' };").unwrap();
    session.run("DEFINE COLLECTION more;").unwrap();
    let error = session
        .run("DEFINE USER intruder ROLE owner PASSWORD 'x';")
        .unwrap_err();
    assert!(
        matches!(error, Error::RoleForbids { role: "editor", .. }),
        "{error}"
    );

    // An owner does all of it.
    session.sign_in("root", "root secret").unwrap();
    session
        .run("DEFINE USER another ROLE viewer PASSWORD 'y';")
        .unwrap();
}

#[test]
fn signing_in_again_replaces_the_identity_rather_than_adding_to_it() {
    let store = store();
    let mut session = ready(&store);
    guarded(&mut session);

    session.sign_in("root", "root secret").unwrap();
    session.sign_in("grace", "watch only").unwrap();
    // A session is one conversation with one user at a time, so the owner's
    // rights do not survive becoming a viewer.
    assert!(session.run("DEFINE COLLECTION more;").is_err());

    session.sign_out();
    assert!(matches!(
        session.run("SELECT * FROM notes;").unwrap_err(),
        Error::NotSignedIn { .. }
    ));
}

#[test]
fn a_scoped_user_cannot_reach_another_tenancy() {
    let store = store();
    let mut session = ready(&store);
    guarded(&mut session);
    session.sign_in("root", "root secret").unwrap();
    session.run("DEFINE NAMESPACE other;").unwrap();

    session.sign_in("ada", "correct horse").unwrap();
    let error = session.run("USE NAMESPACE other;").unwrap_err();
    // The refusal names the tenancy and not a record: one that says whether a
    // record exists has answered the question it declined.
    assert!(matches!(error, Error::OutsideTenancy { .. }), "{error}");
}

#[test]
fn what_is_stored_is_a_hash_and_no_plaintext_reaches_the_log() {
    use tessari_types::Sequence;

    let store = store();
    let mut session = ready(&store);
    session
        .run("DEFINE USER ada ROLE owner PASSWORD 'correct horse';")
        .unwrap();

    // The log is what a replica and a backup receive, so a plaintext there is a
    // plaintext everywhere.
    for (_, record) in store.log_records(Sequence::ZERO, 4096).unwrap() {
        let bytes = format!("{record:?}");
        assert!(
            !bytes.contains("correct horse"),
            "the log carries the password"
        );
    }
}

#[test]
fn a_scoped_user_cannot_reach_another_database_by_naming_it() {
    let store = store();
    let mut session = ready(&store);
    guarded(&mut session);
    session.sign_in("root", "root secret").unwrap();
    session
        .run(
            "DEFINE DATABASE archive;\n\
             USE DATABASE archive;\n\
             DEFINE COLLECTION notes;\n\
             CREATE notes:1 = { body: 'another tenancy' };",
        )
        .unwrap();

    session.sign_in("ada", "correct horse").unwrap();
    session
        .run("USE NAMESPACE prod; USE DATABASE orders;")
        .unwrap();
    // Naming the database directly never touches `USE`, so a check that only
    // guarded `USE` would have guarded the front door of a room with two.
    let error = session.run("SELECT * FROM archive.notes;").unwrap_err();
    assert!(matches!(error, Error::OutsideTenancy { .. }), "{error}");
    // It names the tenancy the author wrote, which leaks nothing they did not
    // already type — and says nothing about whether that table or record exists.
    let said = error.to_string();
    assert!(said.contains("archive"), "{said}");
    assert!(!said.contains("notes"), "{said}");
}

#[test]
fn the_system_namespace_has_no_name_and_so_no_statement_can_reach_it() {
    // Q-18 and Q-26 asked what stops a caller writing into the catalog's own
    // tenancy. The answer is not a guard: namespace zero never claims a name and
    // ids are handed out from one, so no name resolves to it. That is a property
    // of the allocator, which is why it is asserted rather than assumed — a
    // future change to `create_namespace` could take it away silently.
    let store = store();
    let mut session = ready(&store);
    for attempt in ["USE NAMESPACE system;", "USE NAMESPACE catalog;"] {
        session.run(attempt).unwrap();
        let error = session.run("DEFINE COLLECTION intrusion;").unwrap_err();
        assert!(
            matches!(
                error,
                Error::Unknown {
                    entity: "namespace",
                    ..
                }
            ),
            "{attempt} gave {error}"
        );
    }

    let mut transaction = store.begin().unwrap();
    let mut catalog = tessari_storage::Catalog::new(&mut transaction);
    let first = catalog.create_namespace("the very first").unwrap();
    assert_ne!(
        first.id.get(),
        0,
        "the first namespace took the catalog's own id"
    );
    transaction.rollback();
}

#[test]
fn the_plan_chooses_the_index_and_never_the_answer() {
    // The planner is the component most tempted to break the store's governing
    // rule, so the rule is asserted against it directly: the same condition,
    // over the same rows, answers identically whichever candidate is chosen —
    // and the choice is made to differ by declaring the indexes differently.
    fn answered(indexes: &str) -> Vec<RecordId> {
        let store = store();
        let mut session = ready(&store);
        session
            .run(&format!(
                "DEFINE COLLECTION users;\n\
                 {indexes}\n\
                 CREATE users:1 = {{ email: 'a@x', city: 'london', name: 'ada' }};\n\
                 CREATE users:2 = {{ email: 'b@x', city: 'london', name: 'anne' }};\n\
                 CREATE users:3 = {{ email: 'c@x', city: 'paris',  name: 'ada' }};"
            ))
            .unwrap();
        session
            .run("SELECT * FROM users WHERE city = 'london' AND name LIKE 'a%' AND email = 'a@x';")
            .unwrap()[0]
            .records()
            .unwrap()
            .iter()
            .map(|(id, _)| id.clone())
            .collect()
    }

    let expected = vec![RecordId::Int(1)];
    // No index at all: the scan decides, which is the reference answer.
    assert_eq!(answered(""), expected);
    // Each index alone, so each candidate gets its turn at being the only one.
    assert_eq!(answered("DEFINE INDEX i ON users FIELDS city;"), expected);
    assert_eq!(answered("DEFINE INDEX i ON users FIELDS name;"), expected);
    assert_eq!(
        answered("DEFINE INDEX i ON users FIELDS email UNIQUE;"),
        expected
    );
    // And all three, where the plan actually has to choose.
    assert_eq!(
        answered(
            "DEFINE INDEX a ON users FIELDS city;\n\
             DEFINE INDEX b ON users FIELDS name;\n\
             DEFINE INDEX c ON users FIELDS email UNIQUE;"
        ),
        expected
    );
}

#[test]
fn a_filter_reports_the_index_it_used_and_the_scan_when_there_is_none() {
    // The plan is not nameable from outside yet — `AccessPath` says `index`, not
    // *which* — so this asserts the half that is observable: that a servable
    // condition stops being a scan, and an unservable one does not pretend.
    let store = store();
    let mut session = ready(&store);
    session
        .run(
            "DEFINE COLLECTION users;\n\
             DEFINE INDEX by_email ON users FIELDS email UNIQUE;\n\
             CREATE users:1 = { email: 'a@x', city: 'london' };",
        )
        .unwrap();

    let served = session
        .run("SELECT * FROM users WHERE city = 'london' AND email = 'a@x';")
        .unwrap();
    assert_eq!(served[0].path(), Some(AccessPath::Index));

    let scanned = session
        .run("SELECT * FROM users WHERE city = 'london';")
        .unwrap();
    assert_eq!(scanned[0].path(), Some(AccessPath::Scan));
}

#[test]
fn one_statement_observes_one_instant() {
    // A behaviour change, and an improvement, so it is asserted rather than
    // assumed. `time::now()` in a projection used to be evaluated once per
    // record — so a read of a thousand rows could observe a thousand instants,
    // and an `ORDER BY time::now()` would order by the clock rather than by
    // anything the caller asked about. Folding a constant subexpression once per
    // statement is what makes a read mean one moment.
    let store = store();
    let mut session = ready(&store);
    session
        .run(
            "DEFINE COLLECTION users;\n\
             CREATE users:1 = { name: 'ada' };\n\
             CREATE users:2 = { name: 'grace' };\n\
             CREATE users:3 = { name: 'edith' };",
        )
        .unwrap();

    let outcomes = session.run("SELECT time::now() AS at FROM users;").unwrap();
    let records = outcomes[0].records().unwrap();
    assert_eq!(records.len(), 3);
    let first = field(&records[0].1, "at");
    for (_, record) in records {
        assert_eq!(field(record, "at"), first, "one statement, two instants");
    }
}
