//! The container writes on a space key (G079 C6), as `docs/tessariql.md`
//! writes them in "Data structures on a space".

#![allow(clippy::panic, clippy::unwrap_used, clippy::indexing_slicing)]

use tessari_ql::{ExprKind, ListEnd, StatementKind, StructureOp, parse};
use tessari_types::{Number, Value};

fn op(source: &str) -> StructureOp {
    let script = parse(source).unwrap_or_else(|error| panic!("{source}\n  failed: {error}"));
    match script
        .statements
        .into_iter()
        .next()
        .map(|statement| statement.kind)
    {
        Some(StatementKind::Structure { op, .. }) => op,
        other => panic!("{source} parsed as {other:?}"),
    }
}

fn literal(kind: &ExprKind) -> &Value {
    match kind {
        ExprKind::Literal(value) => value,
        other => panic!("not a literal: {other:?}"),
    }
}

#[test]
fn every_documented_container_write_parses() {
    for source in [
        "LIST PUSH queue:'jobs' BACK 'a', 'b';",
        "LIST PUSH queue:'jobs' FRONT 'z' IF PRESENT;",
        "LIST POP queue:'jobs' FRONT;",
        "LIST POP queue:'jobs' BACK LIMIT 10;",
        "LIST SET queue:'jobs' AT -1 = 'last';",
        "LIST TRIM queue:'jobs' FROM 0 TO 99;",
        "LIST REMOVE queue:'jobs' 'a' LIMIT 2 FROM BACK;",
        "LIST INSERT queue:'jobs' 'x' BEFORE 'b';",
        "LIST MOVE queue:'jobs' BACK TO queue:'done' FRONT;",
        "HASH SET carts:'u1' 'apples' = 3, 'pears' = 1;",
        "HASH SET carts:'u1' 'plums' = 2 IF ABSENT;",
        "HASH REMOVE carts:'u1' 'pears';",
        "HASH INCR carts:'u1' 'apples' BY 2;",
        "MEMBERS ADD tags:'post:7' 'rust', 'db';",
        "MEMBERS REMOVE tags:'post:7' 'db';",
        "MEMBERS POP tags:'post:7' LIMIT 2;",
        "MEMBERS MOVE tags:'post:7' 'rust' TO tags:'post:8';",
        "SCORES ADD board:'top' 'ada' = 10, 'bob' = 7;",
        "SCORES ADD board:'top' 'bob' = 9 IF GREATER CHANGED;",
        "SCORES ADD board:'top' 'cy' = 1 IF ABSENT;",
        "SCORES INCR board:'top' 'ada' BY 1.5;",
        "SCORES REMOVE board:'top' 'cy';",
        "SCORES POP board:'top' HIGHEST LIMIT 3;",
    ] {
        op(source);
    }
}

#[test]
fn a_field_before_its_value_is_not_read_as_a_comparison() {
    let StructureOp::HashSet { fields, if_absent } = op("HASH SET carts:'u1' 'a' = 1, 'b' = 2;")
    else {
        panic!("a hash set");
    };
    assert!(!if_absent);
    assert_eq!(fields.len(), 2);
    assert_eq!(literal(&fields[0].0.kind), &Value::from("a"));
    assert_eq!(
        literal(&fields[0].1.kind),
        &Value::Number(Number::Integer(1))
    );
    let StructureOp::ListSet { index, .. } = op("LIST SET q:'l' AT -1 = 'z';") else {
        panic!("a list set");
    };
    assert!(!matches!(index.kind, ExprKind::Binary { .. }), "{index:?}");
    let StructureOp::ListPush { end, values, .. } = op("LIST PUSH q:'l' FRONT 1, 2, 3;") else {
        panic!("a list push");
    };
    assert_eq!((end, values.len()), (ListEnd::Front, 3));
}

#[test]
fn the_heads_stay_ordinary_names() {
    for source in [
        "SELECT list, hash, members, scores FROM t;",
        "CREATE t:1 = { list: [1], hash: 2, members: 3, scores: 4 };",
        "UPDATE t:1 MERGE { list: [] };",
    ] {
        assert!(parse(source).is_ok(), "{source}");
    }
}

#[test]
fn a_wrong_word_after_a_head_names_what_goes_there() {
    for (source, expected) in [
        ("LIST FLY q:'l';", "`PUSH`"),
        ("HASH SET q:'h' 'a' 1;", "`=`"),
        ("SCORES POP q:'z' MIDDLE;", "`LOWEST` or `HIGHEST`"),
        ("LIST PUSH q:'l' SIDEWAYS 1;", "`FRONT` or `BACK`"),
        (
            "SCORES ADD q:'z' 'a' = 1 IF ABSENT IF PRESENT;",
            "each kind once",
        ),
    ] {
        let error = parse(source).expect_err(source).to_string();
        assert!(error.contains(expected), "{source}: {error}");
    }
}
