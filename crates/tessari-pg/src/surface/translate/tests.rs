//! The translator's contract: what each shape of the dialect becomes, and
//! what each shape it does not speak refuses with.

use super::{Columns, UNNAMED, translate};
use tessaridb::Value;

/// The refusal a test expects, printed whole when the code alone mismatches.
fn unwrap_refused(answer: Result<super::Translated, super::Refused>) -> super::Refused {
    match answer {
        Err(refused) => refused,
        Ok(_) => panic!("expected a refusal, got a translation"),
    }
}

fn translated(query: &str) -> (String, Vec<(String, Value)>, Columns) {
    let answer = translate(query).expect("the query should translate");
    let parameters = answer
        .parameters
        .into_iter()
        .collect::<Vec<(String, Value)>>();
    (
        answer.script,
        parameters,
        answer.statements[0].columns.clone(),
    )
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
fn quoted_names_keep_their_case_and_refuse_what_the_store_cannot_hold() {
    // The dialect's quoted spelling preserves case and rides verbatim —
    // the store's lexer names bare words and keeps case, so `"Camel"`
    // reaches a mixed-case table exactly.
    let (script, _, columns) = translated("SELECT \"Camel\" FROM \"MyTable\"");
    assert_eq!(script, "SELECT Camel FROM MyTable;");
    assert_eq!(columns, Columns::Named(vec!["Camel".to_owned()]));
    // A quoted alias answers under its own spelling.
    let (script, _, columns) = translated("SELECT id AS \"OrderId\" FROM t");
    assert_eq!(script, "SELECT id AS OrderId FROM t;");
    assert_eq!(columns, Columns::Named(vec!["OrderId".to_owned()]));
    // Contents the store's name grammar cannot hold refuse by name rather
    // than silently changing the name.
    let refused = translate("SELECT \"has space\" FROM t").expect_err("a spaced name");
    assert_eq!(refused.code, super::super::errors::code::FEATURE);
    let refused = translate("SELECT id FROM \"9lives\"").expect_err("a digit-led name");
    assert_eq!(refused.code, super::super::errors::code::FEATURE);
}

#[test]
fn value_list_in_expands_to_the_exact_or_chain() {
    let (script, parameters, _) = translated("SELECT id FROM orders WHERE band IN (1, 3, 5)");
    assert_eq!(
        script,
        "SELECT id FROM orders WHERE (band = $p1 OR band = $p2 OR band = $p3);"
    );
    assert_eq!(parameters.len(), 3);
    let (script, parameters, _) =
        translated("SELECT id FROM orders WHERE band IN (1, $1) AND name = 'x'");
    assert_eq!(
        script,
        "SELECT id FROM orders WHERE (band = $p1 OR band = $e1) AND name = $p2;"
    );
    assert_eq!(parameters.len(), 2);
    let (script, _, _) = translated("SELECT id FROM orders WHERE band NOT IN (2)");
    assert_eq!(script, "SELECT id FROM orders WHERE NOT (band = $p1);");
    let refused = translate("SELECT id FROM orders WHERE band IN ()").expect_err("empty list");
    assert_eq!(refused.code, super::super::errors::code::SYNTAX);
    // The IN (SELECT …) form stays the engine's own.
    let (script, _, _) = translated("SELECT id FROM orders WHERE band IN (SELECT band FROM bands)");
    assert_eq!(
        script,
        "SELECT id FROM orders WHERE band IN (SELECT band FROM bands);"
    );
}

#[test]
fn projection_arithmetic_renders_with_precedence_and_an_unnamed_alias() {
    let (script, parameters, columns) =
        translated("SELECT band + 1, band * 2 AS doubled, (band + 2) * 3 FROM orders");
    assert_eq!(
        script,
        "SELECT band + $p1 AS e1, band * $p2 AS doubled, (band + $p3) * $p4 AS e2 FROM orders;"
    );
    assert_eq!(parameters.len(), 4);
    assert_eq!(
        columns,
        Columns::Named(vec![
            UNNAMED.to_owned(),
            "doubled".to_owned(),
            UNNAMED.to_owned()
        ])
    );
    // Arithmetic rides the WHERE layer too, and `<>` is the dialect's `!=`.
    let (script, parameters, _) =
        translated("SELECT id FROM orders WHERE band * 2 >= 4 AND name <> 'x'");
    assert_eq!(
        script,
        "SELECT id FROM orders WHERE band * $p1 >= $p2 AND name != $p3;"
    );
    assert_eq!(parameters.len(), 3);
}

#[test]
fn case_and_catalog_functions_refuse_over_the_store_by_name() {
    let refused = unwrap_refused(translate(
        "SELECT CASE band WHEN 1 THEN 'low' END FROM orders",
    ));
    assert_eq!(
        refused.code,
        super::super::errors::code::FEATURE,
        "{refused:?}"
    );
    assert!(refused.message.contains("computed projections"));
    let refused = unwrap_refused(translate(
        "SELECT pg_catalog.pg_get_userbyid(c.relowner) FROM orders",
    ));
    assert_eq!(
        refused.code,
        super::super::errors::code::FEATURE,
        "{refused:?}"
    );
}

#[test]
fn the_translator_collects_parameter_inference() {
    let answer = translate("SELECT id FROM orders WHERE band = $1 AND name = $2 AND band = $1")
        .expect("the query translates");
    assert_eq!(
        answer.statements[0].inference,
        vec![(0_usize, "band".to_owned()), (1_usize, "name".to_owned())]
    );
    // A placeholder without a column comparison collects nothing.
    let answer = translate("SELECT id FROM orders WHERE $1 = 3").expect("the query translates");
    assert!(answer.statements[0].inference.is_empty());
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
    // The extended protocol speaks one statement per Parse, in the dialect's
    // own words.
    let refused = translate("SELECT 1; SELECT 2").expect_err("two statements");
    assert_eq!(refused.code, super::super::errors::code::SYNTAX);
    assert!(refused.message.contains("multiple commands"));
}

#[test]
fn the_simple_query_wraps_several_statements_in_one_block() {
    let translated = super::translate_many("SELECT 1; SELECT name FROM orders").expect("two");
    assert_eq!(
        translated.script,
        "BEGIN; RETURN $p1; SELECT name FROM orders; COMMIT;"
    );
    assert_eq!(translated.statements.len(), 2);
    let single = super::translate_many("SELECT 1;").expect("one");
    assert_eq!(single.script, "RETURN $p1;");
    assert_eq!(single.statements.len(), 1);
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

#[test]
fn group_having_order_render_the_engines_shape() {
    let (script, _, _) = translated(
        "SELECT band, count(*) AS n FROM orders GROUP BY band HAVING count(*) > 1 ORDER BY band DESC",
    );
    assert_eq!(
        script,
        "SELECT band, count(*) AS n FROM orders GROUP BY band HAVING count(*) > 1 ORDER BY band DESC;"
    );
}
