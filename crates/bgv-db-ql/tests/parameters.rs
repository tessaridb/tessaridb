//! Where a parameter may stand, and where it may not.
//!
//! The safety rule is a grammar rule: **a parameter is legal exactly where a
//! literal is**. A value position takes one; a position that names something —
//! a table, a field, an index, a namespace, a user, a role — does not. That is
//! checkable by reading the grammar rather than by auditing the places a value
//! is used, which is why it is the rule and not a convention.

#![allow(clippy::panic, clippy::unwrap_used, clippy::indexing_slicing)]

use std::collections::BTreeMap;

use bgv_db_ql::{Error, Expr, ExprKind, Parameters, Script, Source, StatementKind, parse};
use bgv_db_types::{Number, Value};

fn parsed(source: &str) -> Script {
    match parse(source) {
        Ok(script) => script,
        Err(error) => panic!("{source}\n  failed: {error}"),
    }
}

fn refused(source: &str) -> Error {
    match parse(source) {
        Ok(_) => panic!("{source} parsed, and should not have"),
        Err(error) => error,
    }
}

fn one(name: &str, value: Value) -> Parameters {
    let mut parameters = BTreeMap::new();
    parameters.insert(name.to_owned(), value);
    parameters
}

/// What a single-value statement writes, after binding.
fn written(source: &str, parameters: &Parameters) -> ExprKind {
    let script = parsed(source)
        .bind(parameters)
        .unwrap_or_else(|error| panic!("{source}\n  failed to bind: {error}"));
    match script.statements.into_iter().next().unwrap().kind {
        StatementKind::Create { value, .. }
        | StatementKind::Set { value, .. }
        | StatementKind::Update { value, .. } => value.kind,
        other => panic!("{source} parsed as {other:?}"),
    }
}

fn text(word: &str) -> Value {
    Value::String(word.to_owned())
}

#[test]
fn binding_leaves_a_literal_behind() {
    // The load-bearing property: after binding there is no parameter left in the
    // tree, so nothing downstream — the planner, the evaluator, the index
    // chooser — has to know the feature exists.
    let bound = written(
        "CREATE users:1 = $n;",
        &one("n", Value::Number(Number::Integer(7))),
    );
    assert_eq!(bound, ExprKind::Literal(Value::Number(Number::Integer(7))));
}

#[test]
fn a_parameter_binds_inside_an_object() {
    let ExprKind::Object(fields) =
        written("CREATE users:1 = { name: $x };", &one("x", text("ada")))
    else {
        panic!("not an object");
    };
    assert_eq!(fields[0].value.kind, ExprKind::Literal(text("ada")));
}

#[test]
fn a_parameter_binds_inside_an_array_and_a_set() {
    let parameters = one("x", text("ada"));
    for source in ["CREATE users:1 = [$x];", "CREATE users:1 = set [$x];"] {
        let items = match written(source, &parameters) {
            ExprKind::Array(items) | ExprKind::Set(items) => items,
            other => panic!("{source} parsed as {other:?}"),
        };
        assert_eq!(items[0].kind, ExprKind::Literal(text("ada")));
    }
}

#[test]
fn a_parameter_binds_inside_a_call() {
    let ExprKind::Call { arguments, .. } =
        written("CREATE users:1 = string::len($x);", &one("x", text("ada")))
    else {
        panic!("not a call");
    };
    assert_eq!(arguments[0].kind, ExprKind::Literal(text("ada")));
}

#[test]
fn a_parameter_in_a_condition_is_a_value_and_never_a_route() {
    // `WHERE $field = 3` does *not* read a field named by the caller. A
    // parameter is a value everywhere, which is what keeps a caller who can
    // supply one from naming a column they were never granted.
    let script = parsed("SELECT * FROM users WHERE $field = 3;")
        .bind(&one("field", text("age")))
        .unwrap();
    let StatementKind::Select(select) = &script.statements[0].kind else {
        panic!("not a select");
    };
    let Source::Where { condition, .. } = &select.from else {
        panic!("not a filtered read: {:?}", select.from);
    };
    let Expr {
        kind: ExprKind::Binary { left, .. },
        ..
    } = condition.as_ref()
    else {
        panic!("not a comparison: {condition:?}");
    };
    assert_eq!(left.kind, ExprKind::Literal(text("age")));
}

#[test]
fn an_unbound_parameter_is_named_in_the_refusal() {
    let error = parsed("SELECT * FROM users WHERE name = $who;")
        .bind(&BTreeMap::new())
        .expect_err("an unbound parameter bound");
    assert!(
        matches!(&error, Error::UnboundParameter { name, .. } if name == "who"),
        "{error}"
    );
}

#[test]
fn a_parameter_is_not_a_table() {
    refused("SELECT * FROM $table;");
}

#[test]
fn a_parameter_is_not_a_field_being_declared() {
    refused("DEFINE FIELD $name ON users TYPE string;");
}

#[test]
fn a_parameter_is_not_an_index_name() {
    refused("DEFINE INDEX $name ON users FIELDS email;");
}

#[test]
fn a_parameter_needs_a_name_after_the_marker() {
    refused("CREATE users:1 = $;");
}
