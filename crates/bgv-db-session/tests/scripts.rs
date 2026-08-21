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
fn the_access_path_follows_what_exists_rather_than_how_the_query_is_written() {
    use bgv_db_session::AccessPath;

    let store = store();
    let mut session = ready(&store);
    session
        .run(
            "DEFINE TABLE users;\n\
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
    use bgv_db_session::AccessPath;

    let store = store();
    let mut session = ready(&store);
    session
        .run("DEFINE TABLE users; CREATE users:1 = { email: 'ada@example.com' };")
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
    use bgv_db_session::AccessPath;

    let store = store();
    let mut session = ready(&store);
    session.run("DEFINE TABLE users;").unwrap();
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
            "DEFINE TABLE notes;\n\
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
fn membership_asks_a_different_question_from_a_pattern() {
    let store = store();
    let mut session = ready(&store);
    session
        .run(
            "DEFINE TABLE notes;\n\
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

#[test]
fn a_prefix_pattern_on_an_indexed_field_is_a_range_read_answering_exactly_as_the_scan_did() {
    use bgv_db_session::AccessPath;

    let store = store();
    let mut session = ready(&store);
    session
        .run(
            "DEFINE TABLE people;\n\
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
    use bgv_db_session::AccessPath;

    let store = store();
    let mut session = ready(&store);
    session
        .run(
            "DEFINE TABLE people;\n\
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
    use bgv_db_session::AccessPath;

    let store = store();
    let mut session = ready(&store);
    session
        .run(
            "DEFINE TABLE codes;\n\
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
    use bgv_db_session::AccessPath;

    let store = store();
    let mut session = ready(&store);
    session
        .run(
            "DEFINE TABLE people;\n\
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
            "DEFINE TABLE people;\n\
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
            "DEFINE TABLE events;\n\
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
    session.run("DEFINE TABLE shapes;").unwrap();
    let error = session
        .run("DEFINE FIELD outline ON shapes TYPE geometry;")
        .unwrap_err();
    assert!(error.to_string().contains("type name"), "{error}");
}

#[test]
fn a_schemafull_table_refuses_a_misspelled_field_and_a_schemaless_one_does_not() {
    let store = store();
    let mut session = ready(&store);
    session
        .run(
            "DEFINE TABLE ledger SCHEMAFULL;\n\
             DEFINE FIELD amount ON ledger TYPE decimal;\n\
             DEFINE TABLE notes;\n\
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
            "DEFINE TABLE guests;\n\
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
    session.run("DEFINE TABLE staff;").unwrap();

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
            "DEFINE TABLE members;\n\
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
fn a_traversal_is_an_index_read_and_says_so() {
    use bgv_db_session::AccessPath;

    let store = store();
    let mut session = ready(&store);
    session
        .run(
            "DEFINE TABLE users;\n\
             DEFINE TABLE follows EDGE;\n\
             CREATE users:1 = { handle: 'ada' };\n\
             CREATE users:2 = { handle: 'grace' };\n\
             RELATE users:1->follows->users:2;",
        )
        .unwrap();

    let found = session.run("SELECT * FROM users:1->follows;").unwrap();
    assert_eq!(found[0].path(), Some(AccessPath::Index));

    let reached = session
        .run("SELECT * FROM users:1->follows->users;")
        .unwrap();
    assert_eq!(reached[0].path(), Some(AccessPath::Index));
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
            "DEFINE TABLE users;\n\
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
            "DEFINE TABLE users;\n\
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
            "DEFINE TABLE users;\n\
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
        .run("DEFINE TABLE users;\nDEFINE TABLE follows EDGE;\nCREATE users:1 = { handle: 'ada' };")
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
            "DEFINE TABLE users;\n\
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
            "DEFINE TABLE people;\n\
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
    use bgv_db_session::AccessPath;

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
    use bgv_db_session::AccessPath;

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
    use bgv_db_session::AccessPath;

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
    use bgv_db_session::AccessPath;

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
            AccessPath::Index,
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
        .run("DEFINE TABLE audit; CREATE audit:1 = { who: (SELECT name FROM people:1) };")
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
            "DEFINE TABLE people;\n\
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
            "DEFINE TABLE notes;\n\
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
        .run("DEFINE TABLE audit; CREATE audit:1 = { subject: people };")
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
    use bgv_db_session::AccessPath;

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
    use bgv_db_session::AccessPath;

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
    use bgv_db_session::AccessPath;

    // The right-hand side reads the record, so there is no one value to seek to.
    let store = store();
    let mut session = ready(&store);
    session
        .run(
            "DEFINE TABLE pairs;\n\
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
fn an_ordered_comparison_is_reported_as_a_scan_rather_than_served_as_a_guess() {
    use bgv_db_session::AccessPath;

    // An ordered index could serve `>` as a bounded range and this milestone
    // does not build it. Reported honestly instead of quietly.
    let store = store();
    let mut session = ready(&store);
    mixed_ages(&mut session);
    session
        .run("DEFINE INDEX by_age ON people FIELDS age;")
        .unwrap();

    let found = session.run("SELECT * FROM people WHERE age > 18;").unwrap();
    assert_eq!(found[0].path(), Some(AccessPath::Scan));
}
