//! `DROP … IF EXISTS` on every kind, and `DROP USER` of nobody (ADR-0124 D1).

use tessari_session::{Error, Outcome};

use super::{inside, refused, run, store, value};

/// Every DROP kind that names one object, over a store that holds none of them.
///
/// `t` exists so the kinds that name a table (`INDEX`, `FIELD`, `EVENT`) are
/// asked about the object and not about a missing parent; the missing-parent
/// case is its own test.
const ABSENT: [&str; 25] = [
    "DROP TABLE IF EXISTS gone",
    "DROP SPACE IF EXISTS gone",
    "DROP BUCKET IF EXISTS gone",
    "DROP USER IF EXISTS gone",
    "DROP ANALYZER IF EXISTS gone",
    "DROP SEARCH IF EXISTS gone",
    "DROP SYNONYMS IF EXISTS gone",
    "DROP STOPWORDS IF EXISTS gone",
    "DROP DATABASE IF EXISTS gone",
    "DROP NAMESPACE IF EXISTS gone",
    "DROP GRAPH IF EXISTS gone",
    "DROP EDGE IF EXISTS gone",
    "DROP INDEX IF EXISTS gone ON t",
    "DROP FIELD IF EXISTS gone ON t",
    "DROP KAFKA CONSUMER IF EXISTS gone",
    "DROP REPLICA IF EXISTS gone",
    "DROP VECTOR IF EXISTS gone",
    "DROP GEO IF EXISTS gone",
    "DROP VAULT IF EXISTS gone",
    "DROP QUEUE IF EXISTS gone",
    "DROP TOPIC IF EXISTS gone",
    "DROP TOPIC CONSUMER IF EXISTS gone",
    "DROP SERIES IF EXISTS gone",
    "DROP ROLLUP IF EXISTS gone",
    "DROP VIEW IF EXISTS gone",
];

#[test]
fn every_drop_kind_skips_an_absent_object_when_it_says_if_exists() {
    let store = store();
    let mut session = inside(&store);
    run(
        &mut session,
        "DEFINE TABLE t SCHEMALESS; DEFINE TOPIC news RETAIN 1d;",
    );
    for statement in ABSENT.iter().chain(
        [
            "DROP EVENT IF EXISTS gone ON t",
            "DROP GROUP IF EXISTS 'gone' ON TOPIC news",
        ]
        .iter(),
    ) {
        assert_eq!(
            run(&mut session, &format!("{statement};")),
            Outcome::Done,
            "{statement}"
        );
    }
}

#[test]
fn the_same_drops_without_if_exists_still_refuse() {
    let store = store();
    let mut session = inside(&store);
    run(&mut session, "DEFINE TABLE t SCHEMALESS;");
    // `DROP USER` of nobody has always answered `ok` and still does
    // (`alter_user::dropping_somebody_who_is_not_there_is_still_not_an_error`).
    for statement in ABSENT
        .iter()
        .filter(|statement| !statement.contains("USER"))
    {
        let plain = statement.replace(" IF EXISTS", "");
        let error = refused(&mut session, &format!("{plain};"));
        assert!(matches!(error, Error::Unknown { .. }), "{plain}: {error:?}");
    }
}

#[test]
fn a_missing_parent_is_an_absent_object_too() {
    let store = store();
    let mut session = inside(&store);
    for statement in [
        "DROP INDEX IF EXISTS i ON nowhere;",
        "DROP FIELD IF EXISTS f ON nowhere;",
        "DROP EVENT IF EXISTS e ON nowhere;",
    ] {
        assert_eq!(run(&mut session, statement), Outcome::Done, "{statement}");
    }
}

#[test]
fn if_exists_still_drops_what_is_there() {
    let store = store();
    let mut session = inside(&store);
    run(
        &mut session,
        "DEFINE TABLE t SCHEMALESS; DEFINE INDEX by_n ON t FIELDS n; \
         DEFINE EVENT e ON t THEN CREATE t = { from_event: true };",
    );
    run(
        &mut session,
        "DROP INDEX IF EXISTS by_n ON t; DROP EVENT IF EXISTS e ON t;",
    );
    let info = value(&mut session, "INFO FOR TABLE t;");
    let tessari_types::Value::Object(fields) = info else {
        panic!("{info:?}")
    };
    assert_eq!(
        fields.get("events"),
        Some(&tessari_types::Value::Array(vec![]))
    );
    assert_eq!(
        fields.get("indexes"),
        Some(&tessari_types::Value::Array(vec![]))
    );
    run(&mut session, "DROP TABLE IF EXISTS t;");
    assert!(matches!(
        refused(&mut session, "DROP TABLE t;"),
        Error::Unknown { .. }
    ));
}
