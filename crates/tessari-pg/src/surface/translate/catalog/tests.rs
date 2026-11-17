//! The catalog responder's unit rows: the static relations, the recognized
//! subset, and the refusals that keep the subset honest.

use tessaridb::Value;

use super::{Operand, Term, evaluate};
use crate::surface::translate::parser::Parser;

/// The captured driver query (defer list §1): joins, a bound parameter, the
/// exact projection the client reads back.
const CAPTURED: &str = "SELECT t.typname, t.typtype, t.typelem, r.rngsubtype, \
     t.typbasetype, n.nspname, t.typrelid \
     FROM pg_catalog.pg_type t \
     LEFT OUTER JOIN pg_catalog.pg_range r ON r.rngtypid = t.oid \
     INNER JOIN pg_catalog.pg_namespace n ON t.typnamespace = n.oid \
     WHERE t.oid = $1";

fn parse(query: &str) -> super::Query {
    let mut parser = Parser::new(query);
    assert!(parser.keyword("SELECT").is_ok(), "the test query parses");
    let projection = parser.projection().expect("the projection parses");
    assert!(parser.keyword("FROM").is_ok(), "the FROM rides");
    let word = parser.bare_ident().expect("the schema word");
    super::rest(&mut parser, projection.items, &word)
        .expect("the catalog statement parses")
        .catalog
        .expect("a catalog query")
}

fn bound(oid: &str) -> (Vec<Option<String>>, tessaridb::Parameters) {
    let mut parameters = tessaridb::Parameters::new();
    parameters.insert(
        "e1".to_owned(),
        Value::Number(oid.parse::<i64>().expect("the test oid is a number").into()),
    );
    (vec![Some("e1".to_owned())], parameters)
}

#[test]
fn the_captured_query_answers_the_text_row() {
    let query = parse(CAPTURED);
    let (slots, parameters) = bound("25");
    let rows = evaluate(&query, &slots, &parameters).expect("the query answers");
    assert_eq!(rows.len(), 1);
    let row = &rows[0];
    let cells: Vec<Option<String>> = row
        .iter()
        .map(|cell| cell.as_ref().and_then(crate::surface::types::text_of))
        .collect();
    assert_eq!(
        cells,
        vec![
            Some("text".to_owned()),
            Some("b".to_owned()),
            Some("0".to_owned()),
            None, // the LEFT JOIN finds no range: NULL
            Some("0".to_owned()),
            Some("pg_catalog".to_owned()),
            Some("0".to_owned()),
        ]
    );
}

#[test]
fn an_unbound_type_answers_no_row() {
    let query = parse(CAPTURED);
    let rows = evaluate(&query, &[], &tessaridb::Parameters::new()).expect("the query answers");
    assert!(rows.is_empty(), "no oid, no row");
}

#[test]
fn the_terms_conjoin_and_limit_binds() {
    let query = parse(
        "SELECT t.typname FROM pg_catalog.pg_type t \
         WHERE t.typtype = 'b' AND t.typnamespace = 11 LIMIT 3",
    );
    let rows = evaluate(&query, &[], &tessaridb::Parameters::new()).expect("the query answers");
    assert_eq!(rows.len(), 3, "the limit binds");
}

#[test]
fn is_null_terms_read_the_left_join_padding() {
    let query = parse(
        "SELECT t.typname FROM pg_catalog.pg_type t \
         LEFT OUTER JOIN pg_catalog.pg_range r ON r.rngtypid = t.oid \
         WHERE r.rngsubtype IS NULL LIMIT 1",
    );
    let rows = evaluate(&query, &[], &tessaridb::Parameters::new()).expect("the query answers");
    assert_eq!(rows.len(), 1, "every type lacks a range");
}

#[test]
fn a_group_by_refuses_by_name() {
    // The statement layer owns the end: GROUP BY is "the rest of the query".
    let refused =
        super::super::translate("SELECT t.typtype FROM pg_catalog.pg_type t GROUP BY t.typtype")
            .expect_err("GROUP BY is not spoken");
    assert_eq!(refused.code, crate::surface::errors::code::SYNTAX);
}

#[test]
fn placeholder_slots_map_bound_values() {
    let query = parse("SELECT t.typname FROM pg_catalog.pg_type t WHERE t.oid = $1");
    assert!(
        matches!(
            query.terms.first(),
            Some(Term::Eq {
                operand: Operand::Param(0),
                ..
            })
        ),
        "the placeholder took slot one"
    );
}
