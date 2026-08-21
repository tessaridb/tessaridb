//! The parser, tested against the specification's own examples.
//!
//! Every statement here is copied from `docs/bgvql.md` rather than invented, so
//! that the document and the parser cannot drift apart quietly — which is the
//! failure mode a hand-written grammar actually has. The parser accepting
//! something the document does not describe raises nothing anywhere.

#![allow(clippy::panic, clippy::unwrap_used, clippy::indexing_slicing)]

use bgv_db_ql::{Error, ExprKind, RecordTarget, Script, Source, StatementKind, parse};
use bgv_db_types::{Datetime, Number, RecordId, Value};

fn script(source: &str) -> Script {
    match parse(source) {
        Ok(parsed) => parsed,
        Err(error) => panic!("{source}\n  failed: {error}"),
    }
}

fn one(source: &str) -> StatementKind {
    let parsed = script(source);
    assert_eq!(parsed.statements.len(), 1, "{source}");
    parsed.statements.into_iter().next().unwrap().kind
}

/// The value a single-expression statement writes.
fn written(source: &str) -> ExprKind {
    match one(source) {
        StatementKind::Create { value, .. }
        | StatementKind::Set { value, .. }
        | StatementKind::Update { value, .. } => value.kind,
        other => panic!("{source} parsed as {other:?}"),
    }
}

fn literal(source: &str) -> Value {
    match written(source) {
        ExprKind::Literal(value) => value,
        other => panic!("{source} parsed as {other:?}"),
    }
}

// ---------------------------------------------------------------- §2 session

#[test]
fn the_session_statement_takes_either_half_or_both() {
    assert!(matches!(
        one("USE NAMESPACE prod;"),
        StatementKind::Use {
            namespace: Some(_),
            database: None
        }
    ));
    assert!(matches!(
        one("USE DATABASE orders;"),
        StatementKind::Use {
            namespace: None,
            database: Some(_)
        }
    ));
    let StatementKind::Use {
        namespace: Some(namespace),
        database: Some(database),
    } = one("USE NAMESPACE prod DATABASE orders;")
    else {
        panic!("expected both halves");
    };
    assert_eq!(namespace.text, "prod");
    assert_eq!(database.text, "orders");
}

#[test]
fn a_session_statement_that_selects_nothing_is_refused() {
    let error = parse("USE;").unwrap_err();
    assert!(matches!(error, Error::UnexpectedToken { .. }), "{error}");
}

// --------------------------------------------------------------- §3 literals

#[test]
fn every_literal_in_the_specification_parses_to_the_value_it_names() {
    assert_eq!(literal("SET k:1 = NONE"), Value::None);
    assert_eq!(literal("SET k:1 = NULL"), Value::Null);
    assert_eq!(literal("SET k:1 = true"), Value::Bool(true));
    assert_eq!(literal("SET k:1 = false"), Value::Bool(false));
    assert_eq!(literal("SET k:1 = 42"), Value::Number(Number::Integer(42)));
    assert_eq!(literal("SET k:1 = -7"), Value::Number(Number::Integer(-7)));
    assert_eq!(literal("SET k:1 = 1.5"), Value::Number(Number::float(1.5)));
    assert_eq!(
        literal("SET k:1 = 1e10"),
        Value::Number(Number::float(1e10))
    );
    assert_eq!(
        literal("SET k:1 = 'text'"),
        Value::String("text".to_owned())
    );
    assert_eq!(
        literal("SET k:1 = \"text\""),
        Value::String("text".to_owned())
    );
    assert_eq!(literal("SET k:1 = 0x0a1b"), Value::Bytes(vec![0x0a, 0x1b]));
    assert_eq!(
        literal("SET k:1 = 1h30m"),
        Value::Duration(bgv_db_types::Duration::from_seconds(5400))
    );
    assert_eq!(
        literal("SET k:1 = datetime '1970-01-01T00:00:00Z'"),
        Value::Datetime(Datetime::from_seconds(0))
    );
    assert!(matches!(
        literal("SET k:1 = uuid '550e8400-e29b-41d4-a716-446655440000'"),
        Value::Uuid(_)
    ));
}

#[test]
fn a_decimal_keeps_the_digits_that_were_written() {
    // The whole point of the marker. Reading the decimal back out of the float
    // the lexer produced is the rounding it exists to prevent, so this asserts
    // the exact digits rather than an approximate equality.
    let Value::Number(Number::Decimal(value)) = literal("SET k:1 = dec 12.34") else {
        panic!("expected an exact decimal");
    };
    assert_eq!(value.to_string(), "12.34");

    let Value::Number(Number::Decimal(long)) = literal("SET k:1 = dec 0.1000000000000000055511151")
    else {
        panic!("expected an exact decimal");
    };
    assert_eq!(long.to_string(), "0.1000000000000000055511151");

    // And an unmarked number stays a float, which is the distinction the table
    // in §3 is drawing.
    assert_eq!(
        literal("SET k:1 = 12.34"),
        Value::Number(Number::float(12.34))
    );
}

#[test]
fn containers_hold_expressions_and_tolerate_a_trailing_comma() {
    let ExprKind::Array(items) = written("SET k:1 = ['a', 'b',]") else {
        panic!("expected an array");
    };
    assert_eq!(items.len(), 2);

    let ExprKind::Set(items) = written("SET k:1 = set [1, 2]") else {
        panic!("expected a set");
    };
    assert_eq!(items.len(), 2);

    let ExprKind::Object(fields) = written("CREATE users:1 = { name: 'ada', tags: ['a', 'b'] }")
    else {
        panic!("expected an object");
    };
    assert_eq!(fields[0].name.text, "name");
    assert_eq!(fields[1].name.text, "tags");

    let ExprKind::Range(range) = written("SET k:1 = 1..=10") else {
        panic!("expected a range");
    };
    assert!(range.inclusive);
    assert!(matches!(
        written("SET k:1 = 1..10"),
        ExprKind::Range(range) if !range.inclusive
    ));
}

#[test]
fn a_field_name_may_be_a_reserved_word_or_text() {
    // `unique`, `where`, `range`, `index` and `table` are ordinary words in
    // someone's data. A field name is always followed by `:` and can never be a
    // verb there, so accepting them costs the grammar nothing.
    let ExprKind::Object(fields) =
        written("SET k:1 = { unique: 1, where: 2, table: 3, 'two words': 4 }")
    else {
        panic!("expected an object");
    };
    let names: Vec<&str> = fields
        .iter()
        .map(|field| field.name.text.as_str())
        .collect();
    assert_eq!(names, ["unique", "where", "table", "two words"]);

    // The name comes from the source, so its case survives — a field name is
    // case-sensitive and a keyword is not.
    let ExprKind::Object(fields) = written("SET k:1 = { Unique: 1 }") else {
        panic!("expected an object");
    };
    assert_eq!(fields[0].name.text, "Unique");
}

#[test]
fn an_object_that_names_one_field_twice_is_refused() {
    // Keeping either occurrence stores a value the author did not write, and
    // nothing downstream can tell which one was meant.
    let error = parse("CREATE users:1 = { name: 'ada', name: 'grace' };").unwrap_err();
    assert!(
        matches!(&error, Error::DuplicateField { name, .. } if name == "name"),
        "{error}"
    );
}

#[test]
fn a_record_id_is_one_of_its_four_kinds_and_a_float_is_not_one() {
    for (source, expected) in [
        ("users:1", RecordId::Int(1)),
        ("users:-1", RecordId::Int(-1)),
        ("users:'ada'", RecordId::Text("ada".to_owned())),
        ("users:0x0a1b", RecordId::Bytes(vec![0x0a, 0x1b])),
    ] {
        let StatementKind::Delete { target } = one(&format!("DELETE {source};")) else {
            panic!("{source} did not parse as a delete");
        };
        assert_eq!(target.id, expected, "{source}");
    }

    // `users:1.0` and `users:1` would otherwise be one record or two depending
    // on how the identity was written.
    let error = parse("DELETE users:1.5;").unwrap_err();
    assert!(matches!(error, Error::InvalidRecordId { .. }), "{error}");
}

// ------------------------------------------------------------ §4 definitions

#[test]
fn every_definition_form_parses() {
    assert!(matches!(
        one("DEFINE NAMESPACE prod;"),
        StatementKind::DefineNamespace {
            if_not_exists: false,
            ..
        }
    ));
    assert!(matches!(
        one("DEFINE DATABASE orders;"),
        StatementKind::DefineDatabase { .. }
    ));
    assert!(matches!(
        one("DEFINE TABLE users;"),
        StatementKind::DefineTable { .. }
    ));
    assert!(matches!(
        one("DEFINE SPACE sessions;"),
        StatementKind::DefineSpace { .. }
    ));
    assert!(matches!(
        one("DEFINE TABLE IF NOT EXISTS users;"),
        StatementKind::DefineTable {
            if_not_exists: true,
            ..
        }
    ));

    let StatementKind::DefineIndex {
        name,
        table,
        fields,
        unique,
        if_not_exists,
    } = one("DEFINE INDEX by_email ON users FIELDS email UNIQUE;")
    else {
        panic!("expected an index definition");
    };
    assert_eq!(name.text, "by_email");
    assert_eq!(table.name.text, "users");
    assert_eq!(fields.len(), 1);
    assert_eq!(fields[0].text, "email");
    assert!(unique);
    assert!(!if_not_exists);

    let StatementKind::DefineIndex { fields, unique, .. } =
        one("DEFINE INDEX by_pair ON users FIELDS last, first;")
    else {
        panic!("expected an index definition");
    };
    assert_eq!(fields.len(), 2);
    assert!(!unique, "an index is not unique unless it says so");
}

#[test]
fn both_removals_parse() {
    let StatementKind::DropIndex { name, table } = one("DROP INDEX by_email ON users;") else {
        panic!("expected an index removal");
    };
    assert_eq!(name.text, "by_email");
    assert_eq!(table.name.text, "users");
    assert!(matches!(
        one("DROP TABLE users;"),
        StatementKind::DropTable { .. }
    ));
    // A space is a table, so both spellings reach the same node rather than one
    // of them being a second concept.
    assert!(matches!(
        one("DROP SPACE sessions;"),
        StatementKind::DropTable { .. }
    ));
}

// ----------------------------------------------------------------- §5 reads

#[test]
fn the_three_select_forms_are_the_three_access_paths() {
    let StatementKind::Select(select) = one("SELECT * FROM users:1;") else {
        panic!("expected a read");
    };
    assert!(matches!(select.from, Source::Record(_)));

    let StatementKind::Select(select) = one("SELECT * FROM users;") else {
        panic!("expected a read");
    };
    assert!(matches!(select.from, Source::Table(_)));

    let StatementKind::Select(select) = one("SELECT * FROM users WHERE email = 'ada@example.com';")
    else {
        panic!("expected a read");
    };
    let Source::Index { field, value, .. } = select.from else {
        panic!("expected an index read");
    };
    assert_eq!(field.text, "email");
    assert!(matches!(value.kind, ExprKind::Literal(Value::String(_))));
}

#[test]
fn a_qualified_name_wins_over_the_session() {
    let StatementKind::Select(select) = one("SELECT * FROM orders.users;") else {
        panic!("expected a read");
    };
    let Source::Table(table) = select.from else {
        panic!("expected a table read");
    };
    assert_eq!(
        table.database.map(|name| name.text),
        Some("orders".to_owned())
    );
    assert_eq!(table.name.text, "users");
}

#[test]
fn the_write_statements_replace_a_whole_value() {
    assert!(matches!(
        one("CREATE users:1 = { name: 'ada' };"),
        StatementKind::Create { .. }
    ));
    assert!(matches!(
        one("UPDATE users:1 = { name: 'ada' };"),
        StatementKind::Update { .. }
    ));
    assert!(matches!(
        one("DELETE users:1;"),
        StatementKind::Delete { .. }
    ));
}

// ------------------------------------------------------------- §6 key-value

#[test]
fn the_key_value_verbs_parse() {
    assert!(matches!(
        one("GET sessions:'abc';"),
        StatementKind::Get { .. }
    ));
    assert!(matches!(
        one("DEL sessions:'abc';"),
        StatementKind::Del { .. }
    ));
    assert!(matches!(
        one("SET sessions:'abc' = 42;"),
        StatementKind::Set { .. }
    ));

    let StatementKind::Keys { space, range } = one("KEYS FROM sessions;") else {
        panic!("expected a key listing");
    };
    assert_eq!(space.name.text, "sessions");
    assert!(range.is_none());

    let StatementKind::Keys {
        range: Some(range), ..
    } = one("KEYS FROM sessions RANGE 'a'..'m';")
    else {
        panic!("expected a bounded key listing");
    };
    assert!(!range.inclusive);
}

#[test]
fn a_range_is_required_where_a_range_is_asked_for() {
    let error = parse("KEYS FROM sessions RANGE 'a';").unwrap_err();
    assert!(matches!(error, Error::NotARange { .. }), "{error}");
}

#[test]
fn a_key_value_read_stands_where_a_value_stands() {
    // This is what makes §6's composition claim real rather than aspirational:
    // the read is an expression, so it needs no permission from the statement
    // around it.
    let StatementKind::Select(select) =
        one("SELECT * FROM users WHERE email = GET emails:'primary';")
    else {
        panic!("expected a read");
    };
    let Source::Index { value, .. } = select.from else {
        panic!("expected an index read");
    };
    assert!(matches!(value.kind, ExprKind::Get(_)));

    let ExprKind::Object(fields) = written(
        "CREATE audit:1 = {\n  actor:  GET sessions:'abc',\n  target: (SELECT * FROM users:1),\n}",
    ) else {
        panic!("expected an object");
    };
    assert!(matches!(fields[0].value.kind, ExprKind::Get(_)));
    assert!(matches!(fields[1].value.kind, ExprKind::Select(_)));
}

#[test]
fn a_record_and_a_table_both_stand_where_a_value_stands() {
    let ExprKind::Record(RecordTarget { table, id, .. }) = written("SET sessions:'abc' = users:1")
    else {
        panic!("expected a record");
    };
    assert_eq!(table.name.text, "users");
    assert_eq!(id, RecordId::Int(1));

    assert!(matches!(written("SET k:1 = users"), ExprKind::Table(_)));
}

// ---------------------------------------------------------- §7 transactions

#[test]
fn a_transaction_brackets_the_statements_between_its_ends() {
    let parsed = script(
        "BEGIN;\n  CREATE users:1 = { name: 'ada' };\n  SET sessions:'abc' = users:1;\nCOMMIT;",
    );
    assert_eq!(parsed.statements.len(), 4);
    assert!(matches!(parsed.statements[0].kind, StatementKind::Begin));
    assert!(matches!(parsed.statements[3].kind, StatementKind::Commit));
    assert!(matches!(one("CANCEL;").clone(), StatementKind::Cancel));
}

#[test]
fn statements_need_a_separator_and_the_last_one_may_omit_it() {
    assert_eq!(script("BEGIN; COMMIT").statements.len(), 2);
    assert_eq!(script("BEGIN;COMMIT;").statements.len(), 2);
    assert!(script("").statements.is_empty());

    let error = parse("BEGIN COMMIT").unwrap_err();
    assert!(matches!(error, Error::UnexpectedToken { .. }), "{error}");
}

// ------------------------------------------------- §8 deliberate absences

#[test]
fn what_the_specification_leaves_out_is_refused_by_name() {
    for (source, feature) in [
        ("SELECT * FROM users JOIN orders;", "joins"),
        (
            "SELECT * FROM users GROUP BY email;",
            "aggregation and grouping",
        ),
        ("SELECT * FROM users ORDER BY email;", "ordering a result"),
        ("SELECT * FROM users LIMIT 10;", "limiting a result"),
        ("SELECT name FROM users;", "projecting named fields"),
        ("SET k:1 = (1);", "grouping an expression in parentheses"),
    ] {
        let error = parse(source).unwrap_err();
        let Error::Unsupported { feature: named, .. } = &error else {
            panic!("{source} produced {error}");
        };
        assert_eq!(*named, feature, "{source}");
    }
}

#[test]
fn a_word_that_names_an_absent_feature_is_still_a_legal_name() {
    // The absent features are looked up rather than reserved, so a table called
    // `order` stays usable. Reserving them would break existing data to improve
    // an error message.
    let StatementKind::Select(select) = one("SELECT * FROM order;") else {
        panic!("expected a read");
    };
    assert!(matches!(select.from, Source::Table(_)));
}

// --------------------------------------------------------------- diagnostics

#[test]
fn every_failure_points_at_the_characters_that_caused_it() {
    // `=` is the only comparison the language has, so a `>` is not a wrong
    // operator — it is a character the grammar never uses, and the failure
    // points at it rather than at the statement around it.
    let source = "SELECT * FROM users WHERE email > 'ada';";
    let error = parse(source).unwrap_err();
    let span = error.span();
    assert_eq!(source.get(span.start..span.end), Some(">"), "{error}");

    let source = "CREATE users:1 = { name: };";
    let error = parse(source).unwrap_err();
    let span = error.span();
    assert!(span.start < source.len(), "{error}");
    assert_eq!(source.get(span.start..span.end), Some("}"), "{error}");
}

#[test]
fn a_script_that_ends_mid_statement_says_so_at_the_end() {
    let source = "CREATE users:1 =";
    let error = parse(source).unwrap_err();
    assert!(matches!(error, Error::UnexpectedEnd { .. }), "{error}");
    assert_eq!(error.span().start, source.len());
}

#[test]
fn a_marker_over_text_that_is_not_the_value_it_marks_is_refused() {
    let error = parse("SET k:1 = datetime 'yesterday';").unwrap_err();
    assert!(matches!(error, Error::InvalidDatetime { .. }), "{error}");

    let error = parse("SET k:1 = uuid 'not-a-uuid';").unwrap_err();
    assert!(matches!(error, Error::InvalidUuid { .. }), "{error}");
}

#[test]
fn the_whole_specification_script_parses() {
    let parsed = script(
        "\
        USE NAMESPACE prod DATABASE orders;\n\
        DEFINE TABLE users;\n\
        DEFINE SPACE sessions;\n\
        DEFINE INDEX by_email ON users FIELDS email UNIQUE;\n\
        BEGIN;\n\
        CREATE users:1 = { name: 'ada', email: 'ada@example.com' };\n\
        SET sessions:'abc' = { user: users:1, expires: datetime '2026-09-01T00:00:00Z' };\n\
        COMMIT;\n\
        SELECT * FROM users WHERE email = 'ada@example.com';\n\
        KEYS FROM sessions RANGE 'a'..'m';",
    );
    assert_eq!(parsed.statements.len(), 10);
    assert_eq!(parsed.span.end, parsed.span.end.max(1));
}
