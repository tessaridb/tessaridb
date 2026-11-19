//! The translator's contract: what each shape of the dialect becomes, and
//! what each shape it does not speak refuses with.

#![allow(clippy::panic)]

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
fn offset_pages_the_wider_bounded_read() {
    // G091: OFFSET speaks — the store is asked for the wider read, the port
    // drops the head. The refusal this test used to lock is gone by design.
    let answer = translate("SELECT id FROM t LIMIT 5 OFFSET 3").expect("offset");
    assert_eq!(answer.script, "SELECT id FROM t LIMIT 8;");
    assert_eq!(answer.statements[0].offset, 3);
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

#[test]
fn boolean_literals_bind_in_the_projection() {
    let (script, parameters, columns) = translated("SELECT false AS f, true AS t FROM orders");
    assert_eq!(script, "SELECT $p1 AS f, $p2 AS t FROM orders;");
    assert_eq!(
        parameters,
        vec![
            ("p1".to_owned(), Value::Bool(false)),
            ("p2".to_owned(), Value::Bool(true)),
        ]
    );
    assert_eq!(
        columns,
        Columns::Named(vec!["f".to_owned(), "t".to_owned()])
    );
}

#[test]
fn casts_map_onto_the_store_casts_and_answer_under_the_ports_alias() {
    let (script, _, columns) = translated("SELECT id::text FROM orders");
    assert_eq!(script, "SELECT type::string(id) AS e1 FROM orders;");
    assert_eq!(columns, Columns::Named(vec![UNNAMED.to_owned()]));
    // Chained casts over a qualified path nest left to right (SG2: the cast
    // wave — `::` now translates onto the store's cast functions instead of
    // riding verbatim to a refusal).
    let (script, _, _) = translated("SELECT id::pg_catalog.int4::pg_catalog.text FROM orders");
    assert_eq!(
        script,
        "SELECT type::string(type::int(id)) AS e1 FROM orders;"
    );
}

#[test]
fn the_deep_describes_phase_two_shape_translates() {
    let (script, _, columns) = translated(
        "SELECT c.relchecks, CASE WHEN c.reloftype = 0 THEN '' ELSE c.reloftype::pg_catalog.regtype::pg_catalog.text END, false AS relhasoids FROM pg_catalog.pg_class c WHERE c.oid = '1'",
    );
    // A catalog shape answers locally: no store script, the projection's
    // own names.
    assert_eq!(script, "");
    assert_eq!(
        columns,
        Columns::Named(vec![
            "relchecks".to_owned(),
            UNNAMED.to_owned(),
            "relhasoids".to_owned(),
        ])
    );
}

#[test]
fn catalog_function_calls_parse_with_path_and_literal_args() {
    let (script, _, columns) = translated(
        "SELECT pg_catalog.format_type(a.atttypid, a.atttypmod) FROM pg_catalog.pg_attribute a",
    );
    assert_eq!(script, "");
    assert_eq!(columns, Columns::Named(vec!["format_type".to_owned()]));
}

#[test]
fn projection_subqueries_parse_as_catalog_items() {
    let (script, _, columns) = translated(
        "SELECT a.attname, (SELECT pg_catalog.pg_get_expr(d.adbin, d.adrelid, true) FROM pg_catalog.pg_attrdef d WHERE d.adrelid = a.attrelid AND d.adnum = a.attnum) FROM pg_catalog.pg_attribute a",
    );
    assert_eq!(script, "");
    assert_eq!(
        columns,
        Columns::Named(vec!["attname".to_owned(), "pg_get_expr".to_owned()])
    );
}

#[test]
fn limit_offset_widens_the_bounded_read() {
    let answer = translate("SELECT name FROM orders LIMIT 2 OFFSET 1").expect("translates");
    assert_eq!(answer.script, "SELECT name FROM orders LIMIT 3;");
    assert_eq!(answer.statements[0].offset, 1);
    // OFFSET 0 is the no-op, indistinguishable from no OFFSET at all.
    let answer = translate("SELECT name FROM orders LIMIT 2 OFFSET 0").expect("translates");
    assert_eq!(answer.script, "SELECT name FROM orders LIMIT 2;");
    assert_eq!(answer.statements[0].offset, 0);
}

#[test]
fn offset_alone_reads_unbounded() {
    let answer = translate("SELECT name FROM orders OFFSET 2").expect("translates");
    assert_eq!(answer.script, "SELECT name FROM orders;");
    assert_eq!(answer.statements[0].offset, 2);
}

#[test]
fn limit_all_is_no_bound() {
    let answer = translate("SELECT name FROM orders LIMIT ALL").expect("translates");
    assert_eq!(answer.script, "SELECT name FROM orders;");
    assert_eq!(answer.statements[0].offset, 0);
    let answer = translate("SELECT name FROM orders LIMIT ALL OFFSET 1").expect("translates");
    assert_eq!(answer.script, "SELECT name FROM orders;");
    assert_eq!(answer.statements[0].offset, 1);
}

#[test]
fn offset_composes_with_order_by() {
    let answer = translate("SELECT name FROM orders ORDER BY band DESC LIMIT 1 OFFSET 1")
        .expect("translates");
    assert_eq!(
        answer.script,
        "SELECT name FROM orders ORDER BY band DESC LIMIT 2;"
    );
    assert_eq!(answer.statements[0].offset, 1);
}

#[test]
fn offset_refuses_what_the_number_position_refuses() {
    // A parameter where the literal rides: named, exactly like LIMIT's posture.
    let refused = translate("SELECT name FROM orders LIMIT 1 OFFSET $1").expect_err("placeholder");
    assert_eq!(refused.code, super::super::errors::code::SYNTAX);
    // A negative number is not digits.
    let refused = translate("SELECT name FROM orders OFFSET -1").expect_err("negative");
    assert_eq!(refused.code, super::super::errors::code::SYNTAX);
    // A second OFFSET is the rest of the query, and the query is over.
    let refused = translate("SELECT name FROM orders OFFSET 1 OFFSET 2").expect_err("twice");
    assert_eq!(refused.code, super::super::errors::code::SYNTAX);
    // The projection-without-FROM shape owes no clauses: OFFSET there wants a FROM.
    let refused = translate("SELECT 1 OFFSET 1").expect_err("no FROM");
    assert_eq!(refused.code, super::super::errors::code::SYNTAX);
}

#[test]
fn alter_table_add_column_maps_onto_add_field() {
    let (script, parameters, _) = translated("ALTER TABLE orders ADD COLUMN qty int");
    assert_eq!(script, "ALTER TABLE orders ADD FIELD qty TYPE int;");
    assert!(parameters.is_empty());
    // COLUMN is the dialect's optional noise word.
    let (script, _, _) = translated("ALTER TABLE orders ADD qty int");
    assert_eq!(script, "ALTER TABLE orders ADD FIELD qty TYPE int;");
    // The type correspondence is CREATE TABLE's, both directions.
    let (script, _, _) = translated("ALTER TABLE orders ADD COLUMN ref uuid");
    assert_eq!(script, "ALTER TABLE orders ADD FIELD ref TYPE uuid;");
    // NOT NULL is the engine's REQUIRED; DEFAULT rides inline, written not bound.
    let (script, parameters, _) =
        translated("ALTER TABLE orders ADD COLUMN note text NOT NULL DEFAULT 'n/a'");
    assert_eq!(
        script,
        "ALTER TABLE orders ADD FIELD note TYPE string REQUIRED DEFAULT 'n/a';"
    );
    assert!(
        parameters.is_empty(),
        "DEFAULT is written, not bound: {parameters:?}"
    );
    // A parameter where the default rides refuses by name.
    let refused = translate("ALTER TABLE orders ADD COLUMN note text DEFAULT $1")
        .expect_err("a bound default");
    assert_eq!(refused.code, super::super::errors::code::FEATURE);
    // An unmapped type refuses by name, as it does in CREATE TABLE.
    let refused =
        translate("ALTER TABLE orders ADD COLUMN points point").expect_err("an unmapped type");
    assert_eq!(refused.code, super::super::errors::code::FEATURE);
}

#[test]
fn alter_table_drop_column_maps_onto_drop_field_with_its_notice() {
    let answer = translate("ALTER TABLE orders DROP COLUMN qty").expect("translates");
    assert_eq!(answer.script, "DROP FIELD qty ON orders;");
    assert_eq!(
        answer.statements[0].tag,
        super::super::translate::write::Tag::AlterTable
    );
    assert!(
        answer.statements[0].notice.is_some(),
        "the kept values ride a notice, never silence"
    );
    // COLUMN is optional here too.
    let (script, _, _) = translated("ALTER TABLE orders DROP qty");
    assert_eq!(script, "DROP FIELD qty ON orders;");
    // IF EXISTS is not spoken: the engine verb carries no such flag.
    let refused = translate("ALTER TABLE orders DROP COLUMN IF EXISTS qty").expect_err("IF EXISTS");
    assert_eq!(refused.code, super::super::errors::code::FEATURE);
    let refused = translate("ALTER TABLE orders ADD COLUMN IF NOT EXISTS qty int")
        .expect_err("IF NOT EXISTS");
    assert_eq!(refused.code, super::super::errors::code::FEATURE);
}

#[test]
fn alter_table_renames_ride_the_engine_verbs() {
    let (script, _, _) = translated("ALTER TABLE orders RENAME COLUMN name TO title");
    assert_eq!(script, "ALTER TABLE orders RENAME FIELD name TO title;");
    let (script, _, _) = translated("ALTER TABLE orders RENAME name TO title");
    assert_eq!(script, "ALTER TABLE orders RENAME FIELD name TO title;");
    let (script, _, _) = translated("ALTER TABLE orders RENAME TO purchases");
    assert_eq!(script, "ALTER TABLE orders RENAME TO purchases;");
}

#[test]
fn alter_table_alter_column_type_and_the_refuse_list() {
    let (script, _, _) = translated("ALTER TABLE orders ALTER COLUMN qty TYPE text");
    assert_eq!(script, "ALTER TABLE orders ALTER FIELD qty TYPE string;");
    let (script, _, _) = translated("ALTER TABLE orders ALTER qty TYPE text");
    assert_eq!(script, "ALTER TABLE orders ALTER FIELD qty TYPE string;");
    // USING is not spoken in this round.
    let refused =
        translate("ALTER TABLE orders ALTER COLUMN qty TYPE text USING qty").expect_err("USING");
    assert_eq!(refused.code, super::super::errors::code::FEATURE);
    // The verbs the mapping cannot serve refuse by name.
    let refused = translate("ALTER TABLE orders ADD CONSTRAINT qty CHECK (qty > 0)")
        .expect_err("a constraint");
    assert_eq!(refused.code, super::super::errors::code::FEATURE);
    let refused = translate("ALTER TABLE orders ADD PRIMARY KEY (band)").expect_err("a key");
    assert_eq!(refused.code, super::super::errors::code::FEATURE);
    let refused = translate("ALTER TABLE orders ALTER COLUMN qty SET DEFAULT 1")
        .expect_err("a per-option mutation");
    assert_eq!(refused.code, super::super::errors::code::FEATURE);
    let refused = translate("ALTER TABLE orders ALTER COLUMN qty DROP NOT NULL")
        .expect_err("a per-option mutation");
    assert_eq!(refused.code, super::super::errors::code::FEATURE);
    let refused = translate("ALTER TABLE orders OWNER TO ada").expect_err("an owner change");
    assert_eq!(refused.code, super::super::errors::code::FEATURE);
}

#[test]
fn alter_table_wants_a_known_verb() {
    let refused = translate("ALTER TABLE").expect_err("no table");
    assert_eq!(refused.code, super::super::errors::code::SYNTAX);
    let refused = translate("ALTER TABLE orders").expect_err("no verb");
    assert_eq!(refused.code, super::super::errors::code::SYNTAX);
}

#[test]
fn json_navigation_operators_map_onto_paths() {
    let (script, parameters, _) = translated("SELECT id FROM docs WHERE meta -> 'a' = 1");
    assert_eq!(script, "SELECT id FROM docs WHERE meta['a'] = $p1;");
    assert_eq!(parameters.len(), 1);
    // ->> rides the same path: the wire renders the value as text either way.
    let (script, _, _) = translated("SELECT id FROM docs WHERE meta -> 'a' ->> 'b' = 'x'");
    assert_eq!(script, "SELECT id FROM docs WHERE meta['a']['b'] = $p1;");
    // Numeric keys index arrays; a key with a space rides the bracket spelling.
    let (script, _, _) =
        translated("SELECT id FROM docs WHERE meta -> 'items' -> 0 ->> 'sku tag' = 'a'");
    assert_eq!(
        script,
        "SELECT id FROM docs WHERE meta['items'][0]['sku tag'] = $p1;"
    );
}

#[test]
fn containment_maps_onto_the_document_operator() {
    let (script, parameters, _) = translated("SELECT id FROM docs WHERE meta @> '{\"a\": 1}'");
    assert_eq!(script, "SELECT id FROM docs WHERE meta CONTAINS $p1;");
    assert_eq!(parameters.len(), 1, "the JSON text binds as the object");
    // A parameter on the right rides through.
    let (script, _, _) = translated("SELECT id FROM docs WHERE meta @> $1");
    assert_eq!(script, "SELECT id FROM docs WHERE meta CONTAINS $e1;");
    // Malformed JSON refuses by name.
    let refused = translate("SELECT id FROM docs WHERE meta @> '{unclosed'").expect_err("bad JSON");
    assert_eq!(refused.code, super::super::errors::code::FEATURE);
}

#[test]
fn like_and_ilike_translate_onto_the_store_operators() {
    let (script, parameters, _) = translated("SELECT name FROM t WHERE name LIKE 'a%'");
    assert_eq!(script, "SELECT name FROM t WHERE name LIKE $p1;");
    assert_eq!(parameters.len(), 1);
    let (script, _, _) = translated("SELECT name FROM t WHERE name ILIKE 'a%'");
    assert_eq!(script, "SELECT name FROM t WHERE name ILIKE $p1;");
    let (script, _, _) = translated("SELECT name FROM t WHERE name NOT LIKE 'a%'");
    assert_eq!(script, "SELECT name FROM t WHERE NOT (name LIKE $p1);");
    let (script, _, _) = translated("SELECT name FROM t WHERE name NOT ILIKE 'a%'");
    assert_eq!(script, "SELECT name FROM t WHERE NOT (name ILIKE $p1);");
    // A parameter rides as the pattern.
    let (script, _, _) = translated("SELECT name FROM t WHERE name LIKE $1");
    assert_eq!(script, "SELECT name FROM t WHERE name LIKE $e1;");
}

#[test]
fn the_operator_spellings_map_onto_like_and_ilike() {
    let (script, _, _) = translated("SELECT name FROM t WHERE name ~~ 'a%'");
    assert_eq!(script, "SELECT name FROM t WHERE name LIKE $p1;");
    let (script, _, _) = translated("SELECT name FROM t WHERE name ~~* 'a%'");
    assert_eq!(script, "SELECT name FROM t WHERE name ILIKE $p1;");
    let (script, _, _) = translated("SELECT name FROM t WHERE name !~~ 'a%'");
    assert_eq!(script, "SELECT name FROM t WHERE NOT (name LIKE $p1);");
    let (script, _, _) = translated("SELECT name FROM t WHERE name !~~* 'a%'");
    assert_eq!(script, "SELECT name FROM t WHERE NOT (name ILIKE $p1);");
}

#[test]
fn escape_speaks_the_backslash_and_refuses_the_rest() {
    let (script, _, _) = translated("SELECT name FROM t WHERE name LIKE 'a\\_%' ESCAPE '\\'");
    assert_eq!(script, "SELECT name FROM t WHERE name LIKE $p1;");
    let refused = translate("SELECT name FROM t WHERE name LIKE 'a%' ESCAPE '!'")
        .expect_err("a non-backslash escape");
    assert_eq!(refused.code, super::super::errors::code::FEATURE);
    let refused = translate("SELECT name FROM t WHERE name LIKE 'a%' ESCAPE ''")
        .expect_err("an empty escape");
    assert_eq!(refused.code, super::super::errors::code::FEATURE);
}

#[test]
fn casts_map_onto_the_stores_cast_functions() {
    let (script, _, _) = translated("SELECT id::text FROM orders");
    assert_eq!(script, "SELECT type::string(id) AS e1 FROM orders;");
    let (script, _, _) = translated("SELECT total::float8, active::bool, ref::uuid FROM t");
    assert_eq!(
        script,
        "SELECT type::float(total) AS e1, type::bool(active) AS e2, type::uuid(ref) AS e3 FROM t;"
    );
    let (script, parameters, _) = translated("SELECT id FROM t WHERE amount = '5'::int");
    assert_eq!(script, "SELECT id FROM t WHERE amount = type::int($p1);");
    assert_eq!(parameters.len(), 1);
    let (script, _, _) = translated("SELECT id FROM t WHERE meta = '{\"a\": 1}'::jsonb");
    assert_eq!(script, "SELECT id FROM t WHERE meta = json::parse($p1);");
    let (script, _, _) = translated("SELECT made::timestamptz FROM t");
    assert_eq!(script, "SELECT type::datetime(made) AS e1 FROM t;");
    // Chained casts nest left to right; a catalog-qualified name keeps its
    // last segment.
    let (script, _, _) = translated("SELECT id::int::text FROM t");
    assert_eq!(script, "SELECT type::string(type::int(id)) AS e1 FROM t;");
    let (script, _, _) = translated("SELECT id::pg_catalog.text FROM t");
    assert_eq!(script, "SELECT type::string(id) AS e1 FROM t;");
}

#[test]
fn unspoken_casts_refuse_by_name() {
    for query in [
        "SELECT id::numeric FROM t",
        "SELECT id::decimal FROM t",
        "SELECT id::interval FROM t",
        "SELECT id::regclass FROM t",
        "SELECT id::bytea FROM t",
    ] {
        let refused = translate(query).expect_err("an unspoken cast refuses by name");
        assert_eq!(refused.code, super::super::errors::code::FEATURE, "{query}");
        assert!(
            refused
                .detail
                .as_ref()
                .is_some_and(|detail| !detail.is_empty()),
            "the refusal carries the type it would not speak: {refused:?}"
        );
    }
}

#[test]
fn union_all_passes_the_branches_through() {
    let (script, _, columns) = translated("SELECT id FROM a UNION ALL SELECT id FROM b");
    assert_eq!(script, "SELECT id FROM a UNION ALL SELECT id FROM b;");
    assert_eq!(columns, Columns::Named(vec!["id".to_owned()]));
}

#[test]
fn union_refuses_what_it_cannot_speak_honestly() {
    // DISTINCT is not spoken — the engine's set operations are exact, and a
    // dedup the port cannot spell refuses rather than approximates.
    let refused =
        translate("SELECT id FROM a UNION SELECT id FROM b").expect_err("UNION without ALL");
    assert_eq!(refused.code, super::super::errors::code::FEATURE);
    // A tail on a branch another UNION follows refuses: without parentheses
    // the dialect binds a trailing clause to the whole, and binding it to a
    // middle branch would answer a different question.
    let refused = translate("SELECT id FROM a ORDER BY id UNION ALL SELECT id FROM b")
        .expect_err("a branch tail before UNION");
    assert_eq!(refused.code, super::super::errors::code::FEATURE);
    let refused = translate(
        "SELECT id FROM a UNION ALL SELECT id FROM b ORDER BY id UNION ALL SELECT id FROM c",
    )
    .expect_err("a branch tail between UNIONs");
    assert_eq!(refused.code, super::super::errors::code::FEATURE);
    // A trailing tail on the LAST branch is not the branch's own: it rides
    // verbatim and the engine lifts a trailing ORDER BY onto the collected
    // answer — the compound sorts as one set.
    let (script, _, _) =
        translated("SELECT id FROM a UNION ALL SELECT id FROM b ORDER BY id LIMIT 3");
    assert_eq!(
        script,
        "SELECT id FROM a UNION ALL SELECT id FROM b ORDER BY id LIMIT 3;"
    );
}

#[test]
fn a_values_batch_rides_one_store_insert() {
    // Two rows become ONE store INSERT — one transaction, atomic, the
    // identity list the store answers naming the count.
    let (script, parameters, _) = translated("INSERT INTO t (a, b) VALUES (1, 'x'), (2, 'y')");
    assert_eq!(
        script,
        "INSERT INTO t (a, b) VALUES ($p1, $p2), ($p3, $p4);"
    );
    assert_eq!(parameters.len(), 4);
    // One row stays the CREATE shape the wire has always answered.
    let (script, _, _) = translated("INSERT INTO t (a, b) VALUES (1, 'x')");
    assert_eq!(script, "CREATE t = { a: $p1, b: $p2 } RETURN AFTER;");
    // A batch asking RETURNING refuses by name in this build.
    let refused =
        translate("INSERT INTO t (a) VALUES (1), (2) RETURNING a").expect_err("a batch returning");
    assert_eq!(refused.code, super::super::errors::code::FEATURE);
    // A row of the wrong arity names the row, counted from one.
    let refused =
        translate("INSERT INTO t (a, b) VALUES (1, 'x'), (2, 'y', 3)").expect_err("bad arity");
    assert_eq!(refused.code, super::super::errors::code::SYNTAX);
    assert!(
        refused
            .detail
            .as_deref()
            .is_some_and(|detail| detail.contains("row 2")),
        "the refusal names the row: {refused:?}"
    );
}
