//! The parser, tested against the specification's own examples.
//!
//! Every statement here is copied from `docs/tessariql.md` rather than invented, so
//! that the document and the parser cannot drift apart quietly — which is the
//! failure mode a hand-written grammar actually has. The parser accepting
//! something the document does not describe raises nothing anywhere.

#![allow(clippy::panic, clippy::unwrap_used, clippy::indexing_slicing)]

use tessari_ql::{
    Approximation, BinaryOp, EdgeClause, Error, ExprKind, Identity, InfoSubject, Projection,
    RecordTarget, Script, Source, StatementKind, parse,
};
use tessari_types::{
    ConflictPolicy, Datetime, FieldKind, Number, RecordId, Replication, ReplicationClass, Value,
};

/// A replication factor, which is never zero.
fn factor(n: u32) -> Replication {
    Replication::Factor(std::num::NonZeroU32::new(n).unwrap())
}

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
        StatementKind::Create { value, .. } | StatementKind::Set { value, .. } => value.kind,
        StatementKind::Update {
            edit: tessari_ql::Edit::Whole(value),
            ..
        } => value.kind,
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
            database: None,
            consumer: None
        }
    ));
    assert!(matches!(
        one("USE DATABASE orders;"),
        StatementKind::Use {
            namespace: None,
            database: Some(_),
            consumer: None
        }
    ));
    let StatementKind::Use {
        namespace: Some(namespace),
        database: Some(database),
        consumer: None,
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
        Value::Duration(tessari_types::Duration::from_seconds(5400))
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
        let StatementKind::Delete { target, .. } = one(&format!("DELETE {source};")) else {
            panic!("{source} did not parse as a delete");
        };
        assert_eq!(target.id, Identity::Fixed(expected), "{source}");
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
        one("DEFINE TABLE users (name string);"),
        StatementKind::DefineTable { .. }
    ));
    assert!(matches!(
        one("DEFINE COLLECTION notes;"),
        StatementKind::DefineCollection { .. }
    ));
    assert!(matches!(
        one("DEFINE SPACE sessions;"),
        StatementKind::DefineSpace { .. }
    ));
    assert!(matches!(
        one("DEFINE TABLE IF NOT EXISTS users SCHEMALESS;"),
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
        ..
    } = one("DEFINE INDEX by_email ON users FIELDS email UNIQUE;")
    else {
        panic!("expected an index definition");
    };
    assert_eq!(name.text, "by_email");
    assert_eq!(table.name.text, "users");
    assert_eq!(fields.len(), 1);
    assert_eq!(fields[0].path.to_string(), "email");
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
    let Source::Where { condition, .. } = select.from else {
        panic!("expected a filtered read");
    };
    let ExprKind::Binary { op, left, right } = condition.kind else {
        panic!("expected a comparison");
    };
    assert_eq!(op, BinaryOp::Equal);
    let ExprKind::Path(field) = left.kind else {
        panic!("expected a path on the left");
    };
    assert_eq!(field.path.to_string(), "email");
    assert!(matches!(right.kind, ExprKind::Literal(Value::String(_))));
}

/// The operator and the route a one-comparison condition is built from.
fn compared(source: &str) -> (BinaryOp, String) {
    let StatementKind::Select(select) = one(source) else {
        panic!("expected a read");
    };
    let Source::Where { condition, .. } = select.from else {
        panic!("expected a filtered read");
    };
    let ExprKind::Binary { op, left, .. } = condition.kind else {
        panic!("expected a comparison");
    };
    let ExprKind::Path(field) = left.kind else {
        panic!("expected a path on the left");
    };
    (op, field.path.to_string())
}

#[test]
fn a_pattern_match_is_its_own_test_rather_than_an_equality() {
    // The caller of this store names its fields — an agent reaches it through a
    // layer that knows its own schema — so the form is the standard one and
    // nothing searches unnamed fields.
    assert_eq!(
        compared("SELECT * FROM notes WHERE body LIKE '%ada%';"),
        (BinaryOp::Like, "body".to_owned())
    );
}

#[test]
fn membership_and_pattern_matching_are_different_tests() {
    // `CONTAINS` asks whether a collection holds a value; `LIKE` asks whether
    // text holds characters. Neither is a spelling of the other.
    assert_eq!(
        compared("SELECT * FROM notes WHERE tags CONTAINS 'urgent';"),
        (BinaryOp::Contains, "tags".to_owned())
    );
    assert_eq!(
        compared("SELECT * FROM notes WHERE tags IN 'urgent';"),
        (BinaryOp::In, "tags".to_owned())
    );
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
    let Source::Where { condition, .. } = select.from else {
        panic!("expected a filtered read");
    };
    let ExprKind::Binary { right, .. } = condition.kind else {
        panic!("expected a comparison");
    };
    assert!(matches!(right.kind, ExprKind::Get(_)));

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
    assert_eq!(id, Identity::Fixed(RecordId::Int(1)));

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
    // `JOIN` used to be the row here and now it is built, so the row moved to
    // what a join still cannot be: outer. A test kept alive by softening what it
    // checks stops being evidence, so it names the absence rather than the
    // clause.
    let source = "SELECT * FROM users LEFT JOIN orders ON users.name = orders.who;";
    let error = parse(source).unwrap_err();
    let Error::Unsupported { feature, .. } = &error else {
        panic!("{source} produced {error}");
    };
    assert_eq!(*feature, "an outer join");
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
    // `@` is a character the grammar never uses, and the failure points at it
    // rather than at the statement around it.
    let source = "SELECT * FROM users WHERE email @ 'ada';";
    let error = parse(source).unwrap_err();
    let span = error.span();
    assert_eq!(source.get(span.start..span.end), Some("@"), "{error}");

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
        DEFINE COLLECTION users;\n\
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

/// The route a filter tests, as it was written.
fn filtered_on(source: &str) -> String {
    match one(source) {
        StatementKind::Select(select) => match select.from {
            Source::Where { condition, .. } => match condition.kind {
                ExprKind::Binary { left, .. } => match left.kind {
                    ExprKind::Path(field) => field.path.to_string(),
                    other => panic!("{source} compared {other:?}"),
                },
                other => panic!("{source} parsed as {other:?}"),
            },
            other => panic!("{source} parsed as {other:?}"),
        },
        other => panic!("{source} parsed as {other:?}"),
    }
}

#[test]
fn a_filter_reads_a_route_into_a_record() {
    assert_eq!(
        filtered_on("SELECT * FROM users WHERE email = 'a';"),
        "email"
    );
    assert_eq!(
        filtered_on("SELECT * FROM users WHERE address.city = 'Paris';"),
        "address.city"
    );
    assert_eq!(
        filtered_on("SELECT * FROM users WHERE tags[0] = 'urgent';"),
        "tags[0]"
    );
    assert_eq!(
        filtered_on("SELECT * FROM users WHERE history[2].by.name = 'ada';"),
        "history[2].by.name"
    );
}

#[test]
fn an_index_projects_routes_as_readily_as_names() {
    let StatementKind::DefineIndex { fields, .. } =
        one("DEFINE INDEX by_home ON users FIELDS address.city, name;")
    else {
        panic!("not an index definition");
    };
    let spelled: Vec<String> = fields.iter().map(|f| f.path.to_string()).collect();
    assert_eq!(spelled, vec!["address.city", "name"]);
}

#[test]
fn a_qualified_table_is_still_a_table_and_not_a_route() {
    // The one place the two rules could collide. `.` qualifies a table by its
    // database in the `FROM` and `ON` positions, and only a filter's left side
    // and an index's projection read a route — so `orders.users` keeps meaning
    // what it has always meant.
    let StatementKind::Select(select) = one("SELECT * FROM orders.users;") else {
        panic!("not a select");
    };
    let Source::Table(table) = select.from else {
        panic!("not a table read");
    };
    assert_eq!(table.database.map(|d| d.text), Some("orders".to_owned()));
    assert_eq!(table.name.text, "users");
}

#[test]
fn a_position_in_a_route_is_a_whole_number_or_the_statement_is_refused() {
    for source in [
        "SELECT * FROM users WHERE tags[-1] = 'a';",
        "SELECT * FROM users WHERE tags['x'] = 'a';",
        "SELECT * FROM users WHERE tags[] = 'a';",
        "SELECT * FROM users WHERE tags[0 = 'a';",
        "DEFINE INDEX by_tag ON users FIELDS tags[a];",
    ] {
        assert!(parse(source).is_err(), "{source} was accepted");
    }
}

/// The names a projection answers under, in the order written.
fn projected_names(source: &str) -> Vec<String> {
    let StatementKind::Select(select) = one(source) else {
        panic!("not a select");
    };
    match select.projection {
        Projection::Values { values, .. } => values.into_iter().map(|v| v.name.text).collect(),
        Projection::All => panic!("{source} projected everything"),
    }
}

#[test]
fn a_projection_is_named_by_the_last_step_of_its_route() {
    assert_eq!(projected_names("SELECT name FROM users;"), ["name"]);
    assert_eq!(projected_names("SELECT address.city FROM users;"), ["city"]);
    assert_eq!(projected_names("SELECT history[0].by FROM users;"), ["by"]);
    assert_eq!(
        projected_names("SELECT name, address.city FROM users;"),
        ["name", "city"]
    );
}

#[test]
fn as_names_a_projection_that_has_no_name_of_its_own() {
    assert_eq!(
        projected_names("SELECT tags[0] AS first_tag FROM users;"),
        ["first_tag"]
    );
    assert_eq!(
        projected_names("SELECT address.city AS home FROM users;"),
        ["home"]
    );

    // A position is not a name, and inventing one would be a convention learned
    // from a surprise.
    let error = parse("SELECT tags[0] FROM users;").unwrap_err();
    assert!(matches!(error, Error::UnnamedProjection { .. }), "{error}");
}

#[test]
fn two_projections_answering_under_one_name_are_refused() {
    // Both would write into one name-ordered object and the last would win, so
    // the read would quietly return half of what it asked for.
    for source in [
        "SELECT address.city, work.city FROM users;",
        "SELECT name, email AS name FROM users;",
        "SELECT a.x AS n, b.y AS n FROM users;",
    ] {
        let error = parse(source).unwrap_err();
        let Error::DuplicateProjection { name, .. } = &error else {
            panic!("{source} produced {error}");
        };
        assert!(!name.is_empty());
    }
}

#[test]
fn the_wildcard_is_still_the_whole_record() {
    let StatementKind::Select(select) = one("SELECT * FROM users;") else {
        panic!("not a select");
    };
    assert_eq!(select.projection, Projection::All);
}

#[test]
fn conditions_bind_in_the_order_the_specification_states() {
    // `OR` loosest, then `AND`, then `NOT`, then the comparisons. Asserted on
    // the shape of the tree rather than on what it answers, because the answer
    // would be the same for several wrong shapes over the right data.
    let StatementKind::Select(select) = one("SELECT * FROM users WHERE a = 1 AND b = 2 OR c = 3;")
    else {
        panic!("not a select");
    };
    let Source::Where { condition, .. } = select.from else {
        panic!("not a filtered read");
    };
    let ExprKind::Or(left, right) = condition.kind else {
        panic!("`OR` should be outermost");
    };
    assert!(matches!(left.kind, ExprKind::And(_, _)));
    assert!(matches!(right.kind, ExprKind::Binary { .. }));
}

#[test]
fn parentheses_say_the_other_thing() {
    let StatementKind::Select(select) =
        one("SELECT * FROM users WHERE a = 1 AND (b = 2 OR c = 3);")
    else {
        panic!("not a select");
    };
    let Source::Where { condition, .. } = select.from else {
        panic!("not a filtered read");
    };
    let ExprKind::And(_, right) = condition.kind else {
        panic!("`AND` should be outermost");
    };
    assert!(matches!(right.kind, ExprKind::Or(_, _)));
}

#[test]
fn two_comparisons_in_a_row_are_refused_rather_than_read_as_one_of_them() {
    // `1 < age < 100` means "between" to a person and `(1 < age) < 100` to a
    // parser. A grammar that silently picks one answers a question nobody asked.
    assert!(parse("SELECT * FROM users WHERE 1 < age < 100;").is_err());
}

#[test]
fn a_name_reads_as_a_route_in_a_condition_and_a_table_in_a_value() {
    let StatementKind::Select(select) = one("SELECT * FROM users WHERE users = 3;") else {
        panic!("not a select");
    };
    let Source::Where { condition, .. } = select.from else {
        panic!("not a filtered read");
    };
    let ExprKind::Binary { left, .. } = condition.kind else {
        panic!("not a comparison");
    };
    assert!(matches!(left.kind, ExprKind::Path(_)));

    // The same word, in a value position, is the table.
    assert!(matches!(
        written("CREATE audit:1 = users"),
        ExprKind::Table(_)
    ));

    // And with a `:` after it, a record — in either position.
    let StatementKind::Select(select) = one("SELECT * FROM users WHERE owner = users:1;") else {
        panic!("not a select");
    };
    let Source::Where { condition, .. } = select.from else {
        panic!("not a filtered read");
    };
    let ExprKind::Binary { right, .. } = condition.kind else {
        panic!("not a comparison");
    };
    assert!(matches!(right.kind, ExprKind::Record(_)));
}

#[test]
fn a_fetch_clause_is_read_where_it_is_applied() {
    // The grammar keeps clause order and application order the same, so the
    // routes are followed before anything groups, projects or sorts — and the
    // statement is written that way round too.
    let StatementKind::Select(select) =
        one("SELECT * FROM posts FETCH author, meta.editor ORDER BY author.name LIMIT 3;")
    else {
        panic!("not a select");
    };
    assert_eq!(select.fetch.len(), 2);
    assert_eq!(select.fetch[0].path.to_string(), "author");
    assert_eq!(select.fetch[1].path.to_string(), "meta.editor");
    assert_eq!(select.order.len(), 1);
    assert_eq!(select.limit, Some(3));
}

#[test]
fn a_read_without_the_clause_fetches_nothing() {
    let StatementKind::Select(select) = one("SELECT * FROM posts;") else {
        panic!("not a select");
    };
    assert!(select.fetch.is_empty());
}

#[test]
fn fetch_is_contextual_so_it_is_still_a_name() {
    // A field called `fetch`, projected and filtered on, in a statement that
    // also carries the clause.
    let StatementKind::Select(select) = one("SELECT fetch FROM posts FETCH author;") else {
        panic!("not a select");
    };
    assert_eq!(select.fetch.len(), 1);
    let Projection::Values { values: wanted, .. } = &select.projection else {
        panic!("not a named projection");
    };
    assert_eq!(wanted[0].name.text, "fetch");
}

#[test]
fn an_edge_table_parses_its_endpoints_its_properties_and_the_order_its_edges_are_held_in() {
    let StatementKind::DefineTable {
        name,
        columns,
        edge,
        if_not_exists,
        ..
    } = one("DEFINE TABLE follows (at datetime) EDGE FROM users TO users ORDER BY at DESC;")
    else {
        panic!("not a table");
    };
    assert_eq!(name.text, "follows");
    let Some(EdgeClause::Between(declared)) = edge else {
        panic!("not a declared pair");
    };
    assert_eq!(declared.from.name.text, "users");
    assert_eq!(declared.to.name.text, "users");
    assert_eq!(columns.len(), 1);
    assert_eq!(columns[0].name.text, "at");
    let order = declared.order.as_ref().expect("the order was written");
    assert_eq!(order.field.text, "at");
    assert!(order.descending);
    assert!(!if_not_exists);
}

#[test]
fn a_bare_edge_table_keeps_the_meaning_it_had_before_the_clause_existed() {
    // The clause is optional, so every edge table already written keeps parsing
    // and keeps accepting a link between any two records. That is the whole
    // compatibility contract of this wave, and it is asserted rather than
    // assumed.
    let StatementKind::DefineTable { edge, columns, .. } = one("DEFINE TABLE follows EDGE;") else {
        panic!("not a table");
    };
    assert!(matches!(edge, Some(EdgeClause::Any)));
    assert!(columns.is_empty());
}

#[test]
fn a_table_that_is_only_a_link_declares_no_properties_and_no_order() {
    let StatementKind::DefineTable { columns, edge, .. } =
        one("DEFINE TABLE wrote EDGE FROM users TO posts;")
    else {
        panic!("not a table");
    };
    assert!(columns.is_empty());
    let Some(EdgeClause::Between(declared)) = edge else {
        panic!("not a declared pair");
    };
    assert!(declared.order.is_none());
}

#[test]
fn an_edge_ordering_defaults_to_ascending_and_says_so_the_same_way_a_read_does() {
    // Written and unwritten reach the same value, which is what makes `ASC`
    // safe to accept: a reader spelling the default out gets the default.
    for source in [
        "DEFINE TABLE follows (at datetime) EDGE FROM users TO users ORDER BY at ASC;",
        "DEFINE TABLE follows (at datetime) EDGE FROM users TO users ORDER BY at;",
    ] {
        let StatementKind::DefineTable { edge, .. } = one(source) else {
            panic!("not a table");
        };
        let Some(EdgeClause::Between(declared)) = edge else {
            panic!("not a declared pair");
        };
        assert!(
            !declared.order.expect("the order was written").descending,
            "{source}"
        );
    }
}

#[test]
fn half_a_declared_pair_is_refused_naming_the_word_that_is_missing() {
    // `FROM` without `TO` is refused, and the refusal says which word it wanted:
    // "not allowed" without "write this instead" turns a one-word fix into a
    // search through the specification.
    let error = parse("DEFINE TABLE follows EDGE FROM users;").unwrap_err();
    assert!(error.to_string().contains("TO"), "{error}");
}

#[test]
fn an_edge_ordering_missing_its_by_is_refused_rather_than_read_as_a_column_called_order() {
    let error =
        parse("DEFINE TABLE follows (at datetime) EDGE FROM users TO users ORDER at;").unwrap_err();
    assert!(error.to_string().contains("BY"), "{error}");
}

#[test]
fn order_is_still_an_ordinary_name_because_the_clause_word_is_contextual() {
    // The clause is read with contextual words, so an edge table may carry a
    // property called `order` and a table may still be called one.
    let StatementKind::DefineTable { columns, edge, .. } =
        one("DEFINE TABLE ranked (order int) EDGE FROM users TO users;")
    else {
        panic!("not a table");
    };
    assert_eq!(columns[0].name.text, "order");
    let Some(EdgeClause::Between(declared)) = edge else {
        panic!("not a declared pair");
    };
    assert!(declared.order.is_none());
}

#[test]
fn the_word_graph_names_a_container_rather_than_a_pair_of_tables() {
    // It was `DEFINE GRAPH follows FROM users TO users`, which made an edge
    // table wearing a longer word rather than a structure a caller can hold. The
    // clause moved to `DEFINE TABLE … EDGE`, where it always belonged, and the
    // word went to the container — so the endpoint form is refused and the bare
    // one is not.
    assert!(parse("DEFINE GRAPH follows FROM users TO users;").is_err());

    let StatementKind::DefineGraph { name, .. } = one("DEFINE GRAPH social;") else {
        panic!("not a graph");
    };
    assert_eq!(name.text, "social");

    let StatementKind::DropGraph { name } = one("DROP GRAPH social;") else {
        panic!("not a drop");
    };
    assert_eq!(name.text, "social");
}

#[test]
fn depth_takes_a_literal_and_nothing_that_could_be_computed() {
    // The clause exists so that a walk states its own length. A parameter is the
    // case worth naming: `DEPTH $n` parses as nothing here, and that is the
    // point — accepting it would mean a statement whose reach arrives at run
    // time from somewhere a reader of the statement cannot see, which is an
    // unbounded walk with a promise attached.
    let StatementKind::Select(select) = one("SELECT * FROM users:1->follows->users DEPTH 3;")
    else {
        panic!("not a read");
    };
    let Source::Traverse { depth, hops, .. } = &select.from else {
        panic!("not a walk");
    };
    assert_eq!(*depth, Some(3));
    assert_eq!(hops.len(), 1);

    for refused in [
        "SELECT * FROM users:1->follows->users DEPTH $n;",
        "SELECT * FROM users:1->follows->users DEPTH 1 + 2;",
        "SELECT * FROM users:1->follows->users DEPTH 'three';",
        "SELECT * FROM users:1->follows->users DEPTH depth;",
    ] {
        assert!(parse(refused).is_err(), "{refused}");
    }
}

#[test]
fn depth_counts_steps_so_it_starts_at_one() {
    // Refused rather than answered with an empty set: a caller who computed the
    // bound and got zero has a bug, and an empty answer is exactly what would
    // hide it. A negative refuses as the same thing, which it is.
    for refused in [
        "SELECT * FROM users:1->follows->users DEPTH 0;",
        "SELECT * FROM users:1->follows->users DEPTH -1;",
    ] {
        assert!(
            matches!(parse(refused), Err(Error::DepthBelowOne { .. })),
            "{refused}"
        );
    }
}

#[test]
fn depth_needs_one_step_that_lands_somewhere() {
    // Both shapes refuse for one reason: there is no single step to repeat.
    // A chain could mean the whole chain again or its last step again, and a
    // walk ending on the edges has nothing for a second round to start from.
    for refused in [
        "SELECT * FROM users:1->follows->users->follows->users DEPTH 2;",
        "SELECT * FROM users:1->follows DEPTH 2;",
    ] {
        assert!(
            matches!(parse(refused), Err(Error::DepthNeedsOneHopToATable { .. })),
            "{refused}"
        );
    }
}

#[test]
fn an_edge_is_deleted_by_the_pair_it_joins_rather_than_by_its_derived_name() {
    // `RELATE` derives the edge's identity and never shows it, so without this
    // form the only way to remove an edge is to rebuild that string by hand.
    let StatementKind::DeleteEdge {
        from, edges, to, ..
    } = one("DELETE person:1->works_at->company:1;")
    else {
        panic!("not an edge delete");
    };
    assert_eq!(from.table.name.text, "person");
    assert_eq!(edges.name.text, "works_at");
    assert_eq!(to.table.name.text, "company");

    // The record form is untouched: what tells the two apart is the arrow, and
    // a target with no arrow after it is still one record.
    assert!(matches!(
        one("DELETE person:1;"),
        StatementKind::Delete { .. }
    ));
}

#[test]
fn depth_belongs_to_the_source_so_it_is_written_before_the_clauses() {
    // `DEPTH` says how far the walk goes, which is part of what is being read
    // rather than something done to the rows — so it is consumed with the
    // source, and the clauses that shape a read follow it in their usual order.
    let StatementKind::Select(select) =
        one("SELECT * FROM users:1->follows->users DEPTH 2 ORDER BY handle LIMIT 5;")
    else {
        panic!("not a read");
    };
    let Source::Traverse { depth, .. } = &select.from else {
        panic!("not a walk");
    };
    assert_eq!(*depth, Some(2));
    assert_eq!(select.order.len(), 1);

    // And it does not float: written after a clause it is no longer in the
    // position the grammar has for it, which is the same rule that keeps
    // `START` before `LIMIT`.
    assert!(parse("SELECT * FROM users:1->follows->users LIMIT 5 DEPTH 2;").is_err());
}

#[test]
fn a_vector_field_declares_the_width_every_value_must_have() {
    // Both doorways, in one test on purpose: they are the two ways to say the
    // same thing, and a width accepted by one and refused by the other is the
    // failure C5 names. They go through one function, so this asserts that the
    // arrangement is still one function rather than two that agree today.
    let StatementKind::DefineField { kind, .. } =
        one("DEFINE FIELD embedding ON documents TYPE vector<768>;")
    else {
        panic!("not a field declaration");
    };
    assert_eq!(kind, FieldKind::vector(768).unwrap());

    let StatementKind::DefineTable { columns, .. } =
        one("DEFINE TABLE documents (title string, embedding vector<768>);")
    else {
        panic!("not a table declaration");
    };
    assert_eq!(columns.len(), 2);
    assert_eq!(columns[1].kind, FieldKind::vector(768).unwrap());
}

#[test]
fn a_vector_holds_at_least_one_component() {
    // The only value a `vector<0>` field could hold is the empty array, which
    // no distance can measure and no index will keep — a declaration that
    // refuses every write anybody meant to make.
    //
    // A negative width refuses too, but **not here and not as this error**:
    // `<-` is one token in this language, the one an edge walks backwards
    // along, so the width position never sees a minus sign. `vector<-4>` is
    // refused where the `<` was expected, and it says `found <-` — which points
    // at the two characters that are the mistake. Asserted below with the other
    // shapes rather than pretended into this list.
    for refused in [
        "DEFINE FIELD embedding ON documents TYPE vector<0>;",
        "DEFINE TABLE documents (embedding vector<0>);",
        // With a space the `<-` collision does not arise, so a negative width
        // does reach the check — and refuses as what it is, a width below one.
        "DEFINE FIELD embedding ON documents TYPE vector< -4 >;",
    ] {
        assert!(
            matches!(parse(refused), Err(Error::VectorWidthBelowOne { .. })),
            "{refused}"
        );
    }
}

#[test]
fn a_width_is_required_and_is_written_out() {
    // No width-less `vector`: an array whose length nobody declared is the
    // `array` this language already has, and a word that looked checked and was
    // not would be worse than no word.
    //
    // And the width is a **literal**. A width bound at the moment the
    // declaration ran would be a schema whose shape depends on what was passed,
    // while the catalog has to store one answer — the same reason `DEPTH` takes
    // a literal.
    for refused in [
        "DEFINE FIELD embedding ON documents TYPE vector;",
        "DEFINE FIELD embedding ON documents TYPE vector<>;",
        "DEFINE FIELD embedding ON documents TYPE vector<$width>;",
        "DEFINE FIELD embedding ON documents TYPE vector<7.5>;",
        "DEFINE FIELD embedding ON documents TYPE vector<8;",
        "DEFINE FIELD embedding ON documents TYPE vector<-4>;",
        "DEFINE TABLE documents (embedding vector);",
    ] {
        assert!(parse(refused).is_err(), "{refused} was accepted");
    }
}

#[test]
fn vector_stays_a_name_a_caller_may_use() {
    // Contextual, like `order` and `fetch`. A database of embeddings is exactly
    // where a field called `vector` turns up, so reserving the word would take
    // the name away from the callers most likely to want it.
    let StatementKind::DefineField { name, kind, .. } =
        one("DEFINE FIELD vector ON documents TYPE vector<4>;")
    else {
        panic!("not a field declaration");
    };
    assert_eq!(name.text, "vector");
    assert_eq!(kind, FieldKind::vector(4).unwrap());

    let StatementKind::DefineTable { columns, .. } =
        one("DEFINE TABLE documents (vector vector<4>, name string);")
    else {
        panic!("not a table declaration");
    };
    assert_eq!(columns[0].name.text, "vector");
    assert_eq!(columns[0].kind, FieldKind::vector(4).unwrap());
}

#[test]
fn a_vector_store_declares_its_width_and_its_distance() {
    let StatementKind::DefineVector {
        name,
        dimension,
        distance,
        if_not_exists,
    } = one("DEFINE VECTOR embeddings DIMENSION 768 DISTANCE cosine;")
    else {
        panic!("not a vector store declaration");
    };
    assert_eq!(name.text, "embeddings");
    assert_eq!(dimension, 768);
    assert_eq!(distance.text, "cosine");
    assert!(!if_not_exists);

    let StatementKind::DefineVector { if_not_exists, .. } =
        one("DEFINE VECTOR IF NOT EXISTS embeddings DIMENSION 8 DISTANCE euclidean;")
    else {
        panic!("not a vector store declaration");
    };
    assert!(if_not_exists);
}

#[test]
fn a_vector_store_needs_both_clauses_in_the_order_they_are_written() {
    // Neither has a default: a width is the whole capability, and a distance
    // default would decide which queries the store can serve without saying so.
    assert!(parse("DEFINE VECTOR embeddings DISTANCE cosine;").is_err());
    assert!(parse("DEFINE VECTOR embeddings DIMENSION 768;").is_err());
    assert!(parse("DEFINE VECTOR embeddings DISTANCE cosine DIMENSION 768;").is_err());
    assert!(parse("DEFINE VECTOR embeddings;").is_err());
}

#[test]
fn a_stores_width_answers_to_the_same_rules_a_fields_does() {
    // One reader for both, so a number the field refuses is a number the store
    // refuses. The two are asserted together because "one function" is a
    // property of the code that a later change can quietly end.
    assert!(parse("DEFINE VECTOR embeddings DIMENSION 0 DISTANCE cosine;").is_err());
    assert!(parse("DEFINE VECTOR embeddings DIMENSION 70000 DISTANCE cosine;").is_err());
    assert!(parse("DEFINE FIELD e ON t TYPE vector<70000>;").is_err());
    assert!(parse("DEFINE VECTOR embeddings DIMENSION 65536 DISTANCE cosine;").is_ok());
    assert!(parse("DEFINE FIELD e ON t TYPE vector<65536>;").is_ok());
}

#[test]
fn a_vector_store_is_dropped_and_reported_by_the_word_that_made_it() {
    let StatementKind::DropVector { name } = one("DROP VECTOR embeddings;") else {
        panic!("not a vector store drop");
    };
    assert_eq!(name.text, "embeddings");

    let StatementKind::Info { subject, .. } = one("INFO FOR VECTOR embeddings;") else {
        panic!("not an info statement");
    };
    let InfoSubject::Vector(named) = subject else {
        panic!("not a vector subject");
    };
    assert_eq!(named.text, "embeddings");
}

#[test]
fn a_bucket_is_reported_by_the_word_that_made_it() {
    // The bucket was the one engine with no `INFO` subject, which is why the
    // HTTP listing route had no statement to ask about bucket-ness and answered
    // `200` for a plain table instead.
    let StatementKind::Info { subject, .. } = one("INFO FOR BUCKET media;") else {
        panic!("not an info statement");
    };
    let InfoSubject::Bucket(named) = subject else {
        panic!("not a bucket subject");
    };
    assert_eq!(named.text, "media");
}

#[test]
fn vector_stays_a_name_a_caller_may_use_beside_the_new_word() {
    // The word is contextual in all three positions it now appears in, so the
    // table, the field and the store called `vector` all keep working.
    assert!(parse("SELECT vector FROM documents;").is_ok());
    assert!(parse("DEFINE TABLE vector (title string);").is_ok());
    assert!(parse("SELECT * FROM vector;").is_ok());
}

#[test]
fn a_read_may_say_what_it_will_spend_on_an_approximation() {
    let StatementKind::Select(select) = one("SELECT * FROM embeddings \
         ORDER BY vector::cosine(vector, [1.0]) LIMIT 5 APPROXIMATE EFFORT 200;")
    else {
        panic!("not a select");
    };
    assert_eq!(select.approximate, Some(Approximation::Effort(200)));

    // And the bare word still means the engine's own budget rather than none.
    let StatementKind::Select(select) = one("SELECT * FROM embeddings \
         ORDER BY vector::cosine(vector, [1.0]) LIMIT 5 APPROXIMATE;")
    else {
        panic!("not a select");
    };
    assert_eq!(select.approximate, Some(Approximation::Default));

    let StatementKind::Select(select) = one("SELECT * FROM embeddings;") else {
        panic!("not a select");
    };
    assert_eq!(select.approximate, None);
}

#[test]
fn a_budget_without_the_permission_it_qualifies_is_not_expressible() {
    // `EFFORT` stands only after `APPROXIMATE`: an exact scan visits every record
    // by definition and has nothing to spend. Written alone the word is not a
    // clause at all, so the read ends before it and the leftover is refused.
    assert!(parse("SELECT * FROM embeddings LIMIT 5 EFFORT 200;").is_err());
    assert!(parse("SELECT * FROM embeddings LIMIT 5 EFFORT 200 APPROXIMATE;").is_err());
}

#[test]
fn a_walk_keeps_at_least_one_candidate() {
    let error = parse(
        "SELECT * FROM embeddings \
         ORDER BY vector::cosine(vector, [1.0]) LIMIT 5 APPROXIMATE EFFORT 0;",
    )
    .unwrap_err();
    assert!(matches!(error, Error::EffortBelowOne { .. }), "{error}");

    // A budget is written out, not bound: a schema-free number the planner reads
    // before anything is evaluated.
    assert!(parse("SELECT * FROM t LIMIT 5 APPROXIMATE EFFORT $n;").is_err());
}

#[test]
fn effort_stays_a_name_a_caller_may_use() {
    assert!(parse("SELECT effort FROM tasks;").is_ok());
    assert!(parse("DEFINE FIELD effort ON tasks TYPE int;").is_ok());
}

// ------------------------------------------------- §4 namespace replication

/// The clause a namespace declares its copies with, and the one thing it must
/// keep apart: a namespace that said nothing is not a namespace that said
/// `NONE` (ADR-0060). Absence has to survive as a value, because the day a
/// second node exists is the day the difference decides whether the namespace
/// is refused or honoured — and by then the namespaces already exist.
#[test]
fn a_namespace_declares_how_many_copies_it_wants() {
    let StatementKind::DefineNamespace { replication, .. } = one("DEFINE NAMESPACE prod;") else {
        panic!("DEFINE NAMESPACE");
    };
    assert_eq!(replication, None, "a bare definition states nothing");

    let StatementKind::DefineNamespace { replication, .. } =
        one("DEFINE NAMESPACE prod REPLICATION NONE;")
    else {
        panic!("DEFINE NAMESPACE REPLICATION NONE");
    };
    assert_eq!(replication, Some(Replication::None));

    let StatementKind::DefineNamespace { replication, .. } =
        one("DEFINE NAMESPACE prod REPLICATION FACTOR 3;")
    else {
        panic!("DEFINE NAMESPACE REPLICATION FACTOR");
    };
    assert_eq!(replication, Some(factor(3)));
}

/// The clause a table declares its conflict rule with (G027 S3.2).
///
/// On the TABLE and not the namespace, where the writer count sits, because the
/// two are different questions at different levels: a namespace says whether a
/// second writer may exist, a table says what to do when two of them have
/// written one record without seeing each other (Q-633).
#[test]
fn a_table_declares_what_it_does_with_a_write_it_cannot_order() {
    let StatementKind::DefineTable { conflict, .. } = one("DEFINE TABLE ledger (amount int);")
    else {
        panic!("DEFINE TABLE");
    };
    assert_eq!(conflict, None, "a bare definition states nothing");

    let StatementKind::DefineTable { conflict, .. } =
        one("DEFINE TABLE counter (hits int) LAST WRITER WINS;")
    else {
        panic!("DEFINE TABLE … LAST WRITER WINS");
    };
    assert_eq!(conflict, Some(ConflictPolicy::LastWriterWins));

    let StatementKind::DefineTable { conflict, .. } =
        one("DEFINE TABLE ledger (amount int) REFUSE CONFLICTS;")
    else {
        panic!("DEFINE TABLE … REFUSE CONFLICTS");
    };
    assert_eq!(
        conflict,
        Some(ConflictPolicy::Refuse),
        "a stated refusal is not the same fact as a silence, and the grammar \
         has to let an operator say which one they mean"
    );

    // Order-free, like every other adjective on the statement, and it composes
    // with them rather than displacing one.
    let StatementKind::DefineTable {
        conflict,
        identity,
        schemafull,
        ..
    } = one("DEFINE TABLE hits (n int) LAST WRITER WINS SCHEMALESS IDENTITY uuid;")
    else {
        panic!("DEFINE TABLE with three clauses");
    };
    assert_eq!(conflict, Some(ConflictPolicy::LastWriterWins));
    assert_eq!(identity, tessari_types::IdentityKind::Uuid);
    assert!(!schemafull);
}

/// The words the clause is built from reserve nothing.
///
/// `last`, `wins`, `refuse` and `conflicts` are ordinary English that a stored
/// script may already use as a name, and a clause that reserved one would refuse
/// every script that did — silently, at the next upgrade.
#[test]
fn the_conflict_clause_reserves_none_of_its_words() {
    for statement in [
        "DEFINE TABLE last (wins int);",
        "DEFINE TABLE conflicts (refuse int);",
        "DEFINE TABLE writer (last int, wins int, refuse int, conflicts int);",
    ] {
        parse(statement).unwrap_or_else(|error| panic!("{statement} was refused: {error:?}"));
    }
}

/// The clause a namespace declares its **writers** with (G027 S2.1), which is a
/// different question from how many copies it keeps — so it is a different
/// clause and both may stand on one statement.
#[test]
fn a_namespace_declares_how_many_writers_it_admits() {
    let StatementKind::DefineNamespace { class, .. } = one("DEFINE NAMESPACE prod;") else {
        panic!("DEFINE NAMESPACE");
    };
    assert_eq!(class, None, "a bare definition states nothing");

    let StatementKind::DefineNamespace { class, .. } = one("DEFINE NAMESPACE prod MULTI MASTER;")
    else {
        panic!("DEFINE NAMESPACE MULTI MASTER");
    };
    assert_eq!(class, Some(ReplicationClass::MultiMaster));

    let StatementKind::DefineNamespace {
        replication, class, ..
    } = one("DEFINE NAMESPACE prod REPLICATION FACTOR 3 SINGLE LEADER;")
    else {
        panic!("DEFINE NAMESPACE REPLICATION … SINGLE LEADER");
    };
    assert_eq!(replication, Some(factor(3)));
    assert_eq!(class, Some(ReplicationClass::SingleLeader));
}

/// Half a phrase is a mistake, not a namespace named `multi`. The clause is two
/// contextual words, so the second is expected once the first is read — and
/// saying so is what keeps `MULTI` usable as an ordinary name everywhere else.
#[test]
fn half_a_class_clause_is_refused_at_the_word_that_is_missing() {
    assert!(parse("DEFINE NAMESPACE prod MULTI;").is_err());
    assert!(parse("DEFINE NAMESPACE prod SINGLE;").is_err());
}

#[test]
fn the_clause_follows_if_not_exists_rather_than_displacing_it() {
    let StatementKind::DefineNamespace {
        if_not_exists,
        replication,
        ..
    } = one("DEFINE NAMESPACE IF NOT EXISTS prod REPLICATION FACTOR 2;")
    else {
        panic!("DEFINE NAMESPACE IF NOT EXISTS … REPLICATION");
    };
    assert!(if_not_exists);
    assert_eq!(replication, Some(factor(2)));
}

#[test]
fn replication_moves_in_both_directions() {
    // D12: a namespace starts unreplicated and is switched on later, and the
    // statement that switches it on must be able to switch it off again.
    let StatementKind::AlterNamespace { name, replication } =
        one("ALTER NAMESPACE prod REPLICATION FACTOR 3;")
    else {
        panic!("ALTER NAMESPACE REPLICATION FACTOR");
    };
    assert_eq!(name.text, "prod");
    assert_eq!(replication, factor(3));

    let StatementKind::AlterNamespace { replication, .. } =
        one("ALTER NAMESPACE prod REPLICATION NONE;")
    else {
        panic!("ALTER NAMESPACE REPLICATION NONE");
    };
    assert_eq!(replication, Replication::None);
}

#[test]
fn no_copies_at_all_is_refused_where_it_is_written() {
    // A factor of zero decodes as a count and means the data is kept nowhere,
    // so it is refused at the span the author can see rather than stored.
    assert!(parse("DEFINE NAMESPACE prod REPLICATION FACTOR 0;").is_err());
    assert!(parse("ALTER NAMESPACE prod REPLICATION FACTOR 0;").is_err());
}

#[test]
fn an_alter_that_names_nothing_to_change_is_refused() {
    // `ALTER NAMESPACE prod;` has no reading — the statement carries only what
    // it came to change, so there is nothing for it to mean.
    assert!(parse("ALTER NAMESPACE prod;").is_err());
    assert!(parse("ALTER NAMESPACE prod REPLICATION;").is_err());
}

/// `replication` is a contextual word, not a reserved one — so every script
/// written before the clause existed still parses, including the ones that use
/// the word as a name. The 333 `DEFINE NAMESPACE` sites across five
/// repositories are the source set this protects (BGV-FIDELITY-001).
#[test]
fn the_new_words_are_still_usable_as_names() {
    assert!(matches!(
        one("DEFINE TABLE replication (factor int);"),
        StatementKind::DefineTable { .. }
    ));
    assert!(matches!(
        one("DEFINE NAMESPACE factor;"),
        StatementKind::DefineNamespace { .. }
    ));
}

#[test]
fn a_record_is_asked_which_of_its_versions_survive() {
    // G027 S4.3. The sibling of `INFO FOR RECIPIENTS OF`, and parsed the same
    // way: the subject word, `OF`, and a record target.
    assert!(matches!(
        one("INFO FOR VERSIONS OF person:1;"),
        StatementKind::Info {
            subject: InfoSubject::Versions(_),
        }
    ));
}

#[test]
fn the_versions_subject_reserves_none_of_its_words() {
    // `VERSIONS` is contextual, like every other subject word. A table called
    // `versions`, a field called `versions`, and a record in one must all still
    // parse — a subject word that took a name out of circulation would break
    // schemas that predate the statement.
    assert!(matches!(
        one("DEFINE TABLE versions (of int);"),
        StatementKind::DefineTable { .. }
    ));
    assert!(matches!(
        one("SELECT versions FROM audit;"),
        StatementKind::Select(_)
    ));
}

#[test]
fn a_node_is_drained_by_naming_no_roles() {
    // `docs/tessariql.md` § Draining this node. `Roles::NONE` is a state the
    // store has always been able to hold and no statement could ask for: the
    // clause is how an operator takes a node out of service without stopping
    // it. An empty list is the parse, because `named_roles` folds from
    // `Roles::NONE` and therefore already means exactly this.
    let StatementKind::DefineNode { roles, endpoints } = one("DEFINE NODE ROLES NONE;") else {
        panic!("DEFINE NODE ROLES NONE did not parse as DEFINE NODE");
    };
    assert_eq!(roles, Some(Vec::new()), "NONE clears rather than names");
    assert_eq!(endpoints, None, "a clause left out is still left alone");
}

#[test]
fn leaving_the_roles_clause_out_still_leaves_the_roles_alone() {
    // The half that must NOT move. `DEFINE NODE ENDPOINTS …` changing an
    // address without disturbing the roles is why absence cannot also mean
    // clear, and it is the reason `NONE` had to be spelled at all — so the two
    // readings are asserted together rather than in separate tests that could
    // be deleted apart.
    let StatementKind::DefineNode { roles, endpoints } =
        one("DEFINE NODE ENDPOINTS 'db-1.internal:9000';")
    else {
        panic!("DEFINE NODE ENDPOINTS did not parse as DEFINE NODE");
    };
    assert_eq!(roles, None, "an absent clause leaves the field alone");
    assert_eq!(endpoints, Some(vec!["db-1.internal:9000".to_owned()]));
}

#[test]
fn none_is_a_whole_answer_and_not_a_member_of_the_role_list() {
    // A statement naming both would have to decide which one won, and every
    // reading of it is somebody's reasonable expectation. Refused in both
    // orders, because accepting one of them is the failure this guards.
    assert!(parse("DEFINE NODE ROLES NONE, serving;").is_err());
    assert!(parse("DEFINE NODE ROLES serving, NONE;").is_err());
}

#[test]
fn a_peer_keeps_no_second_spelling_for_an_absent_roles_clause() {
    // `DEFINE REPLICA` declares rather than amends, so an absent `ROLES`
    // already means no roles and already drains. Adding `NONE` there would be
    // the second spelling for absent that the specification refuses, and the
    // refusal is asserted rather than intended — the two statements differ
    // because declaring and amending differ, which is exactly the kind of
    // reason that erodes unless something fails when it does.
    assert!(parse("DEFINE REPLICA second AT 'db-2.internal:9000' ROLES NONE;").is_err());
    let StatementKind::DefineReplica { roles, .. } =
        one("DEFINE REPLICA second AT 'db-2.internal:9000';")
    else {
        panic!("DEFINE REPLICA did not parse as DEFINE REPLICA");
    };
    assert_eq!(
        roles, None,
        "absent on a peer, and the store reads it as none"
    );
}
