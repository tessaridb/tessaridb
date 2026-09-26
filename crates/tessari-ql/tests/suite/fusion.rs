//! `ORDER BY FUSE (…) [DEPTH n]` — what it parses to, what renders back, and
//! what is refused by name (G038 S1.1).

#![allow(clippy::panic, clippy::unwrap_used, clippy::expect_used)]

use tessari_ql::test_support::erase_spans;
use tessari_ql::{Fusion, Script, Select, StatementKind, parse, render};
use tessari_types::Number;

fn select(source: &str) -> Select {
    let script = parse(source).expect(source);
    let Some(StatementKind::Select(select)) = script.statements.into_iter().next().map(|s| s.kind)
    else {
        panic!("not a read: {source}");
    };
    *select
}

fn refused(source: &str) -> String {
    parse(source).expect_err(source).to_string()
}

fn erased(source: &str) -> Script {
    let mut script = parse(source).expect(source);
    erase_spans(&mut script);
    script
}

#[test]
fn a_fused_order_holds_its_branches_their_weights_and_its_depth() {
    let read = select(
        "SELECT * FROM notes ORDER BY FUSE (search::score(body, 'lock') DESC, \
         vector::cosine(e, [1, 0]) WEIGHT 2.5) DEPTH 40 LIMIT 10;",
    );
    assert_eq!(read.order.len(), 2);
    assert!(read.order[0].descending);
    assert!(!read.order[1].descending);
    let Some(Fusion { weights, depth, .. }) = read.fusion else {
        panic!("no fusion");
    };
    assert_eq!(weights, vec![Number::Integer(1), Number::float(2.5)]);
    assert_eq!(depth, Some(40));
    assert_eq!(read.limit, Some(10));
}

#[test]
fn fuse_is_a_field_name_unless_a_parenthesis_follows() {
    let read = select("SELECT * FROM t ORDER BY fuse DESC, depth;");
    assert!(read.fusion.is_none());
    assert_eq!(read.order.len(), 2);
    let plain = select("SELECT * FROM t ORDER BY FUSE (a, b);");
    assert_eq!(
        plain.fusion.map(|f| f.depth),
        Some(None),
        "no DEPTH written"
    );
}

#[test]
fn a_fused_order_renders_back_to_the_same_read() {
    for source in [
        "SELECT * FROM t ORDER BY FUSE (a DESC, b WEIGHT 3) DEPTH 20 LIMIT 5;",
        "SELECT * FROM t ORDER BY FUSE (a, b, c WEIGHT 0.5);",
    ] {
        let written = render(&parse(source).unwrap()).unwrap();
        assert!(written.contains("FUSE ("), "{written}");
        assert_eq!(erased(&written), erased(source), "{source} → {written}");
    }
}

#[test]
fn what_a_fused_order_cannot_be_is_refused_by_name() {
    for (source, expected) in [
        (
            "SELECT * FROM t ORDER BY FUSE (a);",
            "at least two orders to fuse",
        ),
        (
            "SELECT * FROM t ORDER BY FUSE (a WEIGHT 0, b);",
            "a number above zero",
        ),
        (
            "SELECT * FROM t ORDER BY FUSE (a WEIGHT -1, b);",
            "a number above zero",
        ),
        (
            "SELECT * FROM t ORDER BY FUSE (a, b) DEPTH 0;",
            "a whole number above zero",
        ),
        (
            "SELECT * FROM t ORDER BY FUSE (a, b), c;",
            "a fused order is the whole order",
        ),
        (
            "SELECT k FROM t GROUP BY k ORDER BY FUSE (a, b);",
            "found GROUP BY",
        ),
        (
            "SELECT * FROM t ORDER BY FUSE (a, b) AFTER t:1;",
            "found AFTER",
        ),
    ] {
        let why = refused(source);
        assert!(why.contains(expected), "{source}: {why}");
    }
}
