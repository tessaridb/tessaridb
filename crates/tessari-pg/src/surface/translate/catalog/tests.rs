//! The catalog responder's unit rows: the static relations, the dynamic
//! user-table rows, the widened recognized subset, and the refusals that
//! keep the subset honest.

use tessaridb::Value;

use super::dynamic::{AttrRow, ClassRow, Dynamic};
use super::evaluate;
use super::terms::{Operand, Term};
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

/// One user table and two columns, as the store's introspection would build
/// it for the tests.
fn dynamic() -> Dynamic {
    Dynamic {
        namespace: ("app".to_owned(), 4_242),
        classes: vec![
            ClassRow {
                oid: 7_777,
                name: "orders".to_owned(),
                hasindex: true,
                hastriggers: false,
            },
            ClassRow {
                oid: 8_888,
                name: "bands".to_owned(),
                hasindex: false,
                hastriggers: true,
            },
        ],
        attributes: vec![
            AttrRow {
                relid: 7_777,
                name: "band".to_owned(),
                type_oid: crate::surface::types::oid::INT8,
                num: 1,
                notnull: false,
                hasdef: false,
                default: None,
            },
            AttrRow {
                relid: 7_777,
                name: "name".to_owned(),
                type_oid: crate::surface::types::oid::TEXT,
                num: 2,
                notnull: false,
                hasdef: true,
                default: Some("'ada'".to_owned()),
            },
        ],
    }
}

#[test]
fn the_captured_query_answers_the_text_row() {
    let query = parse(CAPTURED);
    let (slots, parameters) = bound("25");
    let rows = evaluate(&query, &slots, &parameters, None, None).expect("the query answers");
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
    let rows = evaluate(&query, &[], &tessaridb::Parameters::new(), None, None)
        .expect("the query answers");
    assert!(rows.is_empty(), "no oid, no row");
}

#[test]
fn the_terms_conjoin_and_limit_binds() {
    let query = parse(
        "SELECT t.typname FROM pg_catalog.pg_type t \
         WHERE t.typtype = 'b' AND t.typnamespace = 11 LIMIT 3",
    );
    let rows = evaluate(&query, &[], &tessaridb::Parameters::new(), None, None)
        .expect("the query answers");
    assert_eq!(rows.len(), 3, "the limit binds");
}

#[test]
fn is_null_terms_read_the_left_join_padding() {
    let query = parse(
        "SELECT t.typname FROM pg_catalog.pg_type t \
         LEFT OUTER JOIN pg_catalog.pg_range r ON r.rngtypid = t.oid \
         WHERE r.rngsubtype IS NULL LIMIT 1",
    );
    let rows = evaluate(&query, &[], &tessaridb::Parameters::new(), None, None)
        .expect("the query answers");
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
            Some(Term::Cmp {
                operand: Operand::Param(0),
                ..
            })
        ),
        "the placeholder took slot one"
    );
}

/// The `\dt` shape over the dynamic rows: value-list IN, `<>` and anchored
/// prefix exclusions, the visibility fold, a CASE mapping, the owner
/// function, a quoted alias, and ordinal ordering — the captured psql
/// query, answered.
#[test]
fn the_listing_query_answers_over_user_tables() {
    let query = parse(
        "SELECT n.nspname as \"Schema\", c.relname as \"Name\", \
         CASE c.relkind WHEN 'r' THEN 'table' WHEN 'v' THEN 'view' END as \"Type\", \
         pg_catalog.pg_get_userbyid(c.relowner) as \"Owner\" \
         FROM pg_catalog.pg_class c \
         LEFT JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace \
         LEFT JOIN pg_catalog.pg_am am ON am.oid = c.relam \
         WHERE c.relkind IN ('r','p','') \
         AND n.nspname <> 'pg_catalog' \
         AND n.nspname !~ '^pg_toast' \
         AND n.nspname <> 'information_schema' \
         AND pg_catalog.pg_table_is_visible(c.oid) \
         ORDER BY 1, 2",
    );
    assert!(query.needs_dynamic());
    let rows = evaluate(
        &query,
        &[],
        &tessaridb::Parameters::new(),
        Some(&dynamic()),
        Some("matrix"),
    )
    .expect("the listing answers");
    let cells: Vec<Vec<Option<String>>> = rows
        .iter()
        .map(|row| {
            row.iter()
                .map(|cell| cell.as_ref().and_then(crate::surface::types::text_of))
                .collect()
        })
        .collect();
    assert_eq!(
        cells,
        vec![
            vec![
                Some("app".to_owned()),
                Some("bands".to_owned()),
                Some("table".to_owned()),
                Some("matrix".to_owned())
            ],
            vec![
                Some("app".to_owned()),
                Some("orders".to_owned()),
                Some("table".to_owned()),
                Some("matrix".to_owned())
            ],
        ],
        "alphabetical by the ordinal keys, the owner from the connection"
    );
}

/// The `\d <table>` lookup: the anchored `^(name)$` pattern reads as
/// equality, the namespace join reports the table's schema.
#[test]
fn the_describe_lookup_answers_the_oid_and_schema() {
    let query = parse(
        "SELECT c.oid, n.nspname, c.relname FROM pg_catalog.pg_class c \
         LEFT JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace \
         WHERE c.relname OPERATOR(pg_catalog.~) '^(orders)$' COLLATE pg_catalog.default \
         AND pg_catalog.pg_table_is_visible(c.oid) ORDER BY 2, 3",
    );
    let rows = evaluate(
        &query,
        &[],
        &tessaridb::Parameters::new(),
        Some(&dynamic()),
        None,
    )
    .expect("the lookup answers");
    assert_eq!(rows.len(), 1, "one table matches the anchored name");
    let cells: Vec<Option<String>> = rows[0]
        .iter()
        .map(|cell| cell.as_ref().and_then(crate::surface::types::text_of))
        .collect();
    assert_eq!(
        cells,
        vec![
            Some("7777".to_owned()),
            Some("app".to_owned()),
            Some("orders".to_owned())
        ]
    );
}

/// The attribute listing: a pinned `attrelid` restricts the introspection
/// (the caller reads one table's columns), the ordered comparison and the
/// boolean fold ride, the path ORDER BY sorts numerically.
#[test]
fn the_attribute_listing_reads_the_pinned_table() {
    let query = parse(
        "SELECT a.attname, a.atttypid, a.attnotnull, a.attnum, a.atthasdef, a.attisdropped \
         FROM pg_catalog.pg_attribute a \
         WHERE a.attrelid = $1 AND a.attnum > 0 AND NOT a.attisdropped ORDER BY a.attnum",
    );
    assert!(query.needs_dynamic());
    let (slots, parameters) = bound("7777");
    assert_eq!(
        query.pinned_oid(&slots, &parameters),
        Some(7_777),
        "the bound identity pins one table"
    );
    let rows =
        evaluate(&query, &slots, &parameters, Some(&dynamic()), None).expect("the columns answer");
    let cells: Vec<Vec<Option<String>>> = rows
        .iter()
        .map(|row| {
            row.iter()
                .map(|cell| cell.as_ref().and_then(crate::surface::types::text_of))
                .collect()
        })
        .collect();
    assert_eq!(
        cells,
        vec![
            vec![
                Some("band".to_owned()),
                Some("20".to_owned()),
                Some("f".to_owned()),
                Some("1".to_owned()),
                Some("f".to_owned()),
                Some("f".to_owned())
            ],
            vec![
                Some("name".to_owned()),
                Some("25".to_owned()),
                Some("f".to_owned()),
                Some("2".to_owned()),
                Some("t".to_owned()),
                Some("f".to_owned())
            ],
        ],
        "in attnum order, the store's kinds under the wire oids"
    );
}

#[test]
fn unanchored_patterns_refuse_by_name() {
    let query = parse("SELECT c.oid FROM pg_catalog.pg_class c WHERE c.relname ~ 'ord.r'");
    let refused = evaluate(
        &query,
        &[],
        &tessaridb::Parameters::new(),
        Some(&dynamic()),
        None,
    )
    .expect_err("a pattern outside the anchored shapes refuses");
    assert_eq!(refused.code, crate::surface::errors::code::FEATURE);
    assert!(refused.detail.is_some_and(|d| d.contains("ord.r")));
}

#[test]
fn ordered_comparison_over_a_name_refuses_by_name() {
    let refused =
        super::super::translate("SELECT c.oid FROM pg_catalog.pg_class c WHERE c.relname > 'a'")
            .expect_err("names do not order here");
    assert_eq!(refused.code, crate::surface::errors::code::FEATURE);
    assert!(refused.message.contains("numeric columns"));
}

#[test]
fn kind_oid_maps_the_store_kinds_to_the_wire() {
    use super::dynamic::kind_oid;
    use crate::surface::types::oid;
    assert_eq!(kind_oid("bool"), oid::BOOL);
    assert_eq!(kind_oid("int"), oid::INT8);
    assert_eq!(kind_oid("float"), oid::FLOAT8);
    assert_eq!(kind_oid("decimal"), oid::NUMERIC);
    assert_eq!(kind_oid("number"), oid::NUMERIC);
    assert_eq!(kind_oid("string"), oid::TEXT);
    assert_eq!(kind_oid("datetime"), oid::TIMESTAMPTZ);
    assert_eq!(kind_oid("duration"), oid::INTERVAL);
    assert_eq!(kind_oid("uuid"), oid::UUID);
    assert_eq!(kind_oid("bytes"), oid::BYTEA);
    assert_eq!(kind_oid("object"), oid::TEXT, "the honest fallback");
    // The falsification arm: a planted drift must move a row.
    assert_ne!(
        kind_oid("int"),
        oid::INT4,
        "integers answer as the widest kind"
    );
}

#[test]
fn a_searched_case_maps_rows_through_their_conditions() {
    let query = parse(
        "SELECT typname, CASE WHEN typtype = 'b' THEN 'base' ELSE 'other' END FROM pg_catalog.pg_type",
    );
    let rows = evaluate(&query, &[], &tessaridb::Parameters::new(), None, None)
        .expect("the searched case answers");
    assert!(!rows.is_empty());
    for row in &rows {
        assert_eq!(
            row[1],
            Some(Value::String("base".to_owned())),
            "every static row is a base type: {row:?}"
        );
    }
}

#[test]
fn a_searched_case_arm_casts_through_regtype_to_text() {
    // bool's row takes the THEN arm; the other rows cast their oid through
    // regtype to text — an oid the static catalog holds resolves to the
    // type's name, the rest keep their number.
    let query = parse(
        "SELECT CASE WHEN oid = 16 THEN 'it is bool' ELSE oid::pg_catalog.regtype::pg_catalog.text END FROM pg_catalog.pg_type",
    );
    let rows = evaluate(&query, &[], &tessaridb::Parameters::new(), None, None)
        .expect("the casted arms answer");
    assert_eq!(rows.len(), 16);
    assert_eq!(rows[0][0], Some(Value::String("it is bool".to_owned())));
    assert_eq!(rows[1][0], Some(Value::String("bytea".to_owned())));
    assert_eq!(rows[4][0], Some(Value::String("int4".to_owned())));
    assert_eq!(rows[6][0], Some(Value::String("oid".to_owned())));
}

#[test]
fn the_deep_describes_functions_and_subqueries_refuse_by_name_until_their_wave() {
    let query =
        parse("SELECT pg_catalog.format_type(atttypid, atttypmod) FROM pg_catalog.pg_attribute");
    let refused = evaluate(&query, &[], &tessaridb::Parameters::new(), None, None)
        .expect_err("the function is not spoken yet");
    assert_eq!(refused.code, "0A000", "{refused:?}");

    let query = parse(
        "SELECT (SELECT d.adbin FROM pg_catalog.pg_attrdef d) FROM pg_catalog.pg_attribute a",
    );
    let refused = evaluate(&query, &[], &tessaridb::Parameters::new(), None, None)
        .expect_err("the subquery is not spoken yet");
    assert_eq!(refused.code, "0A000", "{refused:?}");
}

#[test]
fn pg_class_carries_the_deep_describes_columns() {
    let query = parse(
        "SELECT c.relchecks, c.relkind, c.relhasindex, c.relhastriggers, c.relrowsecurity, false AS relhasoids, c.reltablespace, c.relpersistence, c.relreplident FROM pg_catalog.pg_class c WHERE c.oid = '7777'",
    );
    let slots = vec![];
    let mut parameters = tessaridb::Parameters::new();
    parameters.insert("e1".to_owned(), Value::Number(7_777.into()));
    let rows = evaluate(&query, &slots, &parameters, Some(&dynamic()), None)
        .expect("the deep describe's class row answers");
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0][0], Some(Value::Number(0.into())));
    assert_eq!(rows[0][1], Some(Value::String("r".to_owned())));
    assert_eq!(rows[0][2], Some(Value::Bool(true)), "orders holds an index");
    assert_eq!(rows[0][3], Some(Value::Bool(false)));
    assert_eq!(rows[0][4], Some(Value::Bool(false)));
    assert_eq!(rows[0][5], Some(Value::Bool(false)), "the literal rides");
    assert_eq!(rows[0][6], Some(Value::Number(0.into())));
    assert_eq!(rows[0][7], Some(Value::String("p".to_owned())));
    assert_eq!(rows[0][8], Some(Value::String("d".to_owned())));
}

#[test]
fn the_self_join_the_describe_writes_matches_nothing_honestly() {
    // reltoastrelid is 0 for these tables; the LEFT JOIN against pg_class
    // pads NULL rather than inventing a toast relation.
    let query = parse(
        "SELECT c.relname, tc.relname FROM pg_catalog.pg_class c LEFT JOIN pg_catalog.pg_class tc ON (c.reltoastrelid = tc.oid) WHERE c.oid = '7777'",
    );
    let rows = evaluate(
        &query,
        &[],
        &tessaridb::Parameters::new(),
        Some(&dynamic()),
        None,
    )
    .expect("the self join answers");
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0][0], Some(Value::String("orders".to_owned())));
    assert_eq!(rows[0][1], None, "no toast relation: NULL, not a guess");
}

#[test]
fn format_type_names_the_columns_type() {
    let query = parse(
        "SELECT a.attname, pg_catalog.format_type(a.atttypid, a.atttypmod) FROM pg_catalog.pg_attribute a WHERE a.attrelid = '7777'",
    );
    let rows = evaluate(
        &query,
        &[],
        &tessaridb::Parameters::new(),
        Some(&dynamic()),
        None,
    )
    .expect("the type names answer");
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0][1], Some(Value::String("int8".to_owned())));
    assert_eq!(rows[1][1], Some(Value::String("text".to_owned())));
}

#[test]
fn pg_get_expr_hands_the_stored_default_back() {
    let query = parse(
        "SELECT a.attname, (SELECT pg_catalog.pg_get_expr(d.adbin, d.adrelid, true) FROM pg_catalog.pg_attrdef d WHERE d.adrelid = a.attrelid AND d.adnum = a.attnum) FROM pg_catalog.pg_attribute a WHERE a.attrelid = '7777'",
    );
    let rows = evaluate(
        &query,
        &[],
        &tessaridb::Parameters::new(),
        Some(&dynamic()),
        None,
    )
    .expect("the default expressions answer");
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0][0], Some(Value::String("band".to_owned())));
    assert_eq!(rows[0][1], None, "no default: the dialect's NULL");
    assert_eq!(rows[1][0], Some(Value::String("name".to_owned())));
    assert_eq!(
        rows[1][1],
        Some(Value::String("'ada'".to_owned())),
        "the stored default's own text"
    );
}

#[test]
fn the_collation_subquery_names_the_default_for_text_columns() {
    let query = parse(
        "SELECT a.attname, (SELECT c.collname FROM pg_catalog.pg_collation c WHERE c.oid = a.attcollation) FROM pg_catalog.pg_attribute a WHERE a.attrelid = '7777'",
    );
    let rows = evaluate(
        &query,
        &[],
        &tessaridb::Parameters::new(),
        Some(&dynamic()),
        None,
    )
    .expect("the collation names answer");
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0][1], None, "an int column carries no collation: NULL");
    assert_eq!(
        rows[1][1],
        Some(Value::String("default".to_owned())),
        "a text column names the default collation"
    );
}

#[test]
fn offset_skips_the_local_rows_before_the_limit() {
    let query = parse(
        "SELECT t.typname FROM pg_catalog.pg_type t \
         WHERE t.typname = 'bool' OFFSET 1",
    );
    let rows = evaluate(&query, &[], &tessaridb::Parameters::new(), None, None)
        .expect("the query answers");
    assert!(rows.is_empty(), "the only row is dropped: {rows:?}");
    let query = parse(
        "SELECT t.typname FROM pg_catalog.pg_type t \
         WHERE t.typname = 'bool' LIMIT 1 OFFSET 0",
    );
    let rows = evaluate(&query, &[], &tessaridb::Parameters::new(), None, None)
        .expect("the query answers");
    assert_eq!(rows.len(), 1, "OFFSET 0 is the no-op: {rows:?}");
}
