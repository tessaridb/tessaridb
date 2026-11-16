//! The translator's contract: what each shape of the dialect becomes, and
//! what each shape it does not speak refuses with.

use super::{Columns, UNNAMED, translate};
use tessaridb::Value;

fn translated(query: &str) -> (String, Vec<(String, Value)>, Columns) {
    let answer = translate(query).expect("the query should translate");
    let parameters = answer
        .parameters
        .into_iter()
        .collect::<Vec<(String, Value)>>();
    (answer.script, parameters, answer.columns)
}

#[test]
fn star_select_passes_through() {
    let (script, parameters, columns) = translated("SELECT * FROM orders");
    assert_eq!(script, "SELECT * FROM orders;");
    assert!(parameters.is_empty());
    assert_eq!(columns, Columns::Star);
}

#[test]
fn fields_aliases_and_case_folding() {
    let (script, _, columns) = translated("SELECT ID, Name AS Title FROM Orders");
    assert_eq!(script, "SELECT id, name AS title FROM orders;");
    assert_eq!(
        columns,
        Columns::Named(vec!["id".to_owned(), "title".to_owned()])
    );
}

#[test]
fn quoted_names_refuse_by_name() {
    let refused = translate("SELECT \"Camel\" FROM t").expect_err("quoted idents");
    assert_eq!(refused.code, super::super::errors::code::FEATURE);
    let refused = translate("SELECT id FROM \"MyTable\"").expect_err("quoted table");
    assert_eq!(refused.code, super::super::errors::code::FEATURE);
}

#[test]
fn where_literals_bind_and_names_pass() {
    let (script, parameters, _) =
        translated("SELECT id FROM orders WHERE band = 3 AND name = 'ledger'");
    assert_eq!(
        script,
        "SELECT id FROM orders WHERE band = $p1 AND name = $p2;"
    );
    assert_eq!(
        parameters,
        vec![
            ("p1".to_owned(), Value::Number(3.into())),
            ("p2".to_owned(), Value::String("ledger".to_owned())),
        ]
    );
}

#[test]
fn the_dialects_null_is_the_stores_two_absences() {
    let (script, _, _) = translated("SELECT id FROM orders WHERE done IS NULL");
    assert_eq!(
        script,
        "SELECT id FROM orders WHERE (done = NULL OR done = NONE);"
    );
    let (script, _, _) = translated("SELECT id FROM orders WHERE done IS NOT NULL");
    assert_eq!(
        script,
        "SELECT id FROM orders WHERE (done != NULL AND done != NONE);"
    );
}

#[test]
fn or_not_and_parens_keep_their_shape() {
    let (script, _, _) = translated("SELECT id FROM t WHERE NOT (a = 1 OR b > 2) AND c != 3");
    assert_eq!(
        script,
        "SELECT id FROM t WHERE NOT (a = $p1 OR b > $p2) AND c != $p3;"
    );
}

#[test]
fn limit_is_inlined_as_a_checked_number() {
    let (script, parameters, _) = translated("SELECT id FROM t LIMIT 10");
    assert_eq!(script, "SELECT id FROM t LIMIT 10;");
    assert!(parameters.is_empty());
}

#[test]
fn select_without_from_becomes_return() {
    let (script, parameters, columns) = translated("SELECT 1");
    assert_eq!(script, "RETURN $p1;");
    assert_eq!(parameters, vec![("p1".to_owned(), Value::Number(1.into()))]);
    assert_eq!(columns, Columns::Named(vec![UNNAMED.to_owned()]));
}

#[test]
fn float_and_boolean_literals_bind() {
    let (script, parameters, _) = translated("SELECT 1.5, TRUE, FALSE, NULL");
    assert_eq!(script, "RETURN $p1, $p2, $p3, $p4;");
    assert_eq!(parameters.len(), 4);
    assert_eq!(
        parameters[0].1,
        Value::Number(tessaridb::Number::float(1.5))
    );
    assert_eq!(parameters[1].1, Value::Bool(true));
    assert_eq!(parameters[2].1, Value::Bool(false));
    assert_eq!(parameters[3].1, Value::Null);
}

#[test]
fn a_trailing_semicolon_is_punctuation() {
    let (script, _, _) = translated("SELECT id FROM orders;");
    assert_eq!(script, "SELECT id FROM orders;");
}

#[test]
fn a_second_statement_refuses_by_name() {
    let refused = translate("SELECT 1; SELECT 2").expect_err("two statements");
    assert_eq!(refused.code, super::super::errors::code::FEATURE);
    assert!(refused.message.contains("multiple statements"));
}

#[test]
fn offset_refuses_by_name() {
    let refused = translate("SELECT id FROM t LIMIT 5 OFFSET 3").expect_err("offset");
    assert_eq!(refused.code, super::super::errors::code::FEATURE);
}

#[test]
fn a_dotted_table_refuses_by_name() {
    let refused = translate("SELECT id FROM public.orders").expect_err("dotted table");
    assert_eq!(refused.code, super::super::errors::code::FEATURE);
}

#[test]
fn a_broken_shape_is_a_syntax_refusal() {
    let refused = translate("SELECT FROM").expect_err("nothing to select");
    assert_eq!(refused.code, super::super::errors::code::SYNTAX);
    let refused = translate("SELECT id FROM t WHERE").expect_err("no condition");
    assert_eq!(refused.code, super::super::errors::code::SYNTAX);
    let refused =
        translate("SELECT id FROM t WHERE band = 1 FROBNICATE").expect_err("unknown tail");
    assert_eq!(refused.code, super::super::errors::code::SYNTAX);
}

#[test]
fn a_string_escapes_by_doubling() {
    let (script, parameters, _) = translated("SELECT 'it''s'");
    assert_eq!(script, "RETURN $p1;");
    assert_eq!(
        parameters,
        vec![("p1".to_owned(), Value::String("it's".to_owned()))]
    );
}

#[test]
fn where_limit_combined_is_the_wire_shape() {
    let (script, parameters, columns) =
        translated("SELECT name FROM orders WHERE band = 3 LIMIT 1");
    assert_eq!(
        script, "SELECT name FROM orders WHERE band = $p1 LIMIT 1;",
        "{script}"
    );
    assert_eq!(parameters.len(), 1);
    assert_eq!(columns, Columns::Named(vec!["name".to_owned()]));
}
