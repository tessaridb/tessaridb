//! `INSERT INTO t (a, b) VALUES (…), (…)`: the shape, and what it refuses.
//!
//! Its own file rather than a section of `parser.rs`, which holds statements
//! copied out of `docs/tessariql.md` and would otherwise have to be edited
//! before the document describing this statement exists.
//!
//! The property the refusals are about is that the **field list is grammar and
//! the value list is data**. Getting that backwards would put a caller's text
//! where a field name goes, which is the one mistake this language is arranged
//! to make unrepresentable rather than merely discouraged.

#![allow(clippy::panic, clippy::unwrap_used, clippy::indexing_slicing)]

use tessari_ql::{Error, ExprKind, Script, StatementKind, parse};
use tessari_types::Value;

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

fn refused(source: &str) -> Error {
    match parse(source) {
        Ok(_) => panic!("{source} parsed, and should not have"),
        Err(error) => error,
    }
}

#[test]
fn a_single_row_names_its_table_its_fields_and_its_values() {
    let StatementKind::Insert {
        table,
        columns,
        rows,
    } = one("INSERT INTO users (name, email) VALUES ('ada', 'ada@example.com');")
    else {
        panic!("not an insert");
    };
    assert_eq!(table.name.text, "users");
    assert_eq!(
        columns
            .iter()
            .map(|name| name.text.as_str())
            .collect::<Vec<_>>(),
        ["name", "email"]
    );
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].len(), 2);
    assert_eq!(
        rows[0][0].kind,
        ExprKind::Literal(Value::String("ada".to_owned()))
    );
}

#[test]
fn several_rows_are_one_statement_and_keep_the_order_they_were_written() {
    let StatementKind::Insert { rows, .. } =
        one("INSERT INTO users (name) VALUES ('ada'), ('grace'), ('alan');")
    else {
        panic!("not an insert");
    };
    let names: Vec<&ExprKind> = rows.iter().map(|row| &row[0].kind).collect();
    assert_eq!(
        names,
        [
            &ExprKind::Literal(Value::String("ada".to_owned())),
            &ExprKind::Literal(Value::String("grace".to_owned())),
            &ExprKind::Literal(Value::String("alan".to_owned())),
        ]
    );
}

#[test]
fn a_table_may_be_qualified_by_its_database() {
    let StatementKind::Insert { table, .. } =
        one("INSERT INTO orders.users (name) VALUES ('ada');")
    else {
        panic!("not an insert");
    };
    assert_eq!(
        table.database.map(|name| name.text),
        Some("orders".to_owned())
    );
}

#[test]
fn a_short_row_is_refused_at_parse_and_names_both_counts() {
    let error = refused("INSERT INTO users (name, email) VALUES ('ada');");
    assert!(
        matches!(
            &error,
            Error::InsertRowArity {
                row: 1,
                found: 1,
                expected: 2,
                ..
            }
        ),
        "{error}"
    );
}

#[test]
fn a_long_row_is_refused_the_same_way() {
    let error = refused("INSERT INTO users (name) VALUES ('ada', 'extra');");
    assert!(
        matches!(
            &error,
            Error::InsertRowArity {
                found: 2,
                expected: 1,
                ..
            }
        ),
        "{error}"
    );
}

#[test]
fn the_refusal_counts_rows_the_way_a_reader_does() {
    // From one, and it is the *third* row that is wrong — so a message reading
    // "row 2" would send somebody to the line above the mistake.
    let error = refused("INSERT INTO users (name) VALUES ('ada'), ('grace'), ('alan', 'x');");
    assert!(
        matches!(&error, Error::InsertRowArity { row: 3, .. }),
        "{error}"
    );
}

#[test]
fn a_field_position_takes_a_name_and_nothing_else() {
    // A literal cannot stand where a field is named. This is the same rule
    // `DEFINE FIELD` follows, asserted here because `INSERT` is the statement
    // that writes names and values side by side.
    refused("INSERT INTO users ('name') VALUES ('ada');");
}

#[test]
fn a_row_with_no_values_is_refused() {
    refused("INSERT INTO users (name) VALUES ();");
}

#[test]
fn an_insert_with_no_rows_is_refused() {
    refused("INSERT INTO users (name) VALUES;");
}

#[test]
fn the_field_list_is_not_optional() {
    // Without it there is nothing saying which value is which, and a positional
    // reading would depend on a declaration order the caller cannot see.
    refused("INSERT INTO users VALUES ('ada');");
}

#[test]
fn into_is_not_optional() {
    refused("INSERT users (name) VALUES ('ada');");
}

#[test]
fn values_and_into_are_still_usable_as_field_names() {
    // Matched as plain words rather than reserved, the way `BEFORE` and `AFTER`
    // are — so a table with a column called `values` is still writable.
    let StatementKind::Insert { columns, .. } =
        one("INSERT INTO readings (values, into) VALUES (1, 2);")
    else {
        panic!("not an insert");
    };
    assert_eq!(
        columns
            .iter()
            .map(|name| name.text.as_str())
            .collect::<Vec<_>>(),
        ["values", "into"]
    );
}
