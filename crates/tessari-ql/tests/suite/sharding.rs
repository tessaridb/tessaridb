//! `SPLIT AT` — how a table says where its shards begin (G031 S1.1, ADR-0080).
//!
//! The grammar's half only: which points were written, in the order they were
//! written. Whether they are in key order, whether the table's identity allows
//! them, and whether its kind holds records at all are the CATALOG's to refuse,
//! because a parser is the wrong place for an invariant about a stored table —
//! nothing stops a later caller building a definition by hand.

#![allow(clippy::panic, clippy::unwrap_used)]

use tessari_ql::{Error, StatementKind, parse};
use tessari_types::RecordId;

fn split_of(source: &str) -> Vec<RecordId> {
    let parsed = parse(source).unwrap_or_else(|error| panic!("{source}\n  failed: {error}"));
    assert_eq!(parsed.statements.len(), 1, "{source}");
    match parsed.statements.into_iter().next().unwrap().kind {
        StatementKind::DefineTable { split, .. } => split,
        other => panic!("{source} parsed as {other:?}"),
    }
}

fn text(value: &str) -> RecordId {
    RecordId::Text(value.to_owned())
}

#[test]
fn a_table_that_says_nothing_is_one_shard() {
    assert!(split_of("DEFINE TABLE orders (total int);").is_empty());
}

#[test]
fn the_points_are_read_in_the_order_they_were_written() {
    assert_eq!(
        split_of("DEFINE TABLE orders (total int) IDENTITY uuid SPLIT AT 'g', 'p';"),
        vec![text("g"), text("p")]
    );
    // Written out of order on purpose: refusing that is the catalog's job, and a
    // parser that sorted the list would hide the mistake the refusal names.
    assert_eq!(
        split_of("DEFINE TABLE orders (total int) SPLIT AT 'p', 'g';"),
        vec![text("p"), text("g")]
    );
}

#[test]
fn a_point_is_any_identity_a_record_can_have() {
    let points = split_of(
        "DEFINE TABLE t SCHEMALESS SPLIT AT 100, 'm', \
         uuid '0195e0a1-7c2e-7000-8000-000000000000', 0x0102;",
    );
    assert_eq!(points.len(), 4);
    assert_eq!(points[0], RecordId::Int(100));
    assert_eq!(points[1], text("m"));
    assert!(matches!(points[2], RecordId::Uuid(_)));
    assert_eq!(points[3], RecordId::Bytes(vec![1, 2]));
}

#[test]
fn the_clause_composes_with_every_other_adjective_in_any_order() {
    assert_eq!(
        split_of("DEFINE TABLE t (n int) SPLIT AT 'k' SCHEMALESS IDENTITY uuid LAST WRITER WINS;"),
        vec![text("k")]
    );
}

#[test]
fn a_split_point_is_written_in_the_statement_and_never_supplied() {
    // A shard map is a declaration somebody reviews. A point arriving from a
    // parameter is a boundary nobody can read in the script that set it.
    let refused = parse("DEFINE TABLE t (n int) SPLIT AT $from;").unwrap_err();
    assert!(
        matches!(refused, Error::SplitPointIsNotWritten { .. }),
        "a parameter as a split point must be refused by name: {refused:?}"
    );
}

#[test]
fn split_with_no_point_is_refused_rather_than_read_as_one_shard() {
    assert!(matches!(
        parse("DEFINE TABLE t (n int) SPLIT AT;").unwrap_err(),
        Error::InvalidRecordId { .. }
    ));
    assert!(parse("DEFINE TABLE t (n int) SPLIT 'g';").is_err());
}

#[test]
fn the_clause_reserves_neither_of_its_words() {
    for statement in [
        "DEFINE TABLE split (at int);",
        "DEFINE TABLE at (split int);",
        "CREATE split:1 = { at: 1 };",
    ] {
        parse(statement).unwrap_or_else(|error| panic!("{statement} was refused: {error:?}"));
    }
}

#[test]
fn a_subscription_names_one_shard_fully_qualified() {
    let parsed =
        parse("DEFINE REPLICA part AT 'b:9001' REPLICATES SHARD prod.shop.orders 2;").unwrap();
    let StatementKind::DefineReplica {
        replicates: Some(tessari_ql::ReachRef::Shard { table, shard, .. }),
        ..
    } = &parsed.statements[0].kind
    else {
        panic!("{:?}", parsed.statements[0].kind);
    };
    assert_eq!((table.text.as_str(), *shard), ("orders", 2));
    for refused in [
        "DEFINE REPLICA p AT 'b:9001' REPLICATES SHARD prod.shop.orders 0;",
        "DEFINE REPLICA p AT 'b:9001' REPLICATES SHARD prod.shop.orders;",
        "DEFINE REPLICA p AT 'b:9001' REPLICATES SHARD orders 2;",
    ] {
        assert!(parse(refused).is_err(), "{refused}");
    }
}

#[test]
fn a_placement_names_a_namespace_a_database_or_a_shard_and_never_the_store() {
    let parsed = parse(
        "DEFINE REPLICA b AT 'b:9001' REPLICATES STORE LEADS SHARD prod.shop.orders 2; \
         DEFINE REPLICA c AT 'c:9001' LEADS NAMESPACE prod; \
         DEFINE REPLICA d AT 'd:9001' LEADS DATABASE prod.shop;",
    )
    .unwrap();
    let leads: Vec<_> = parsed
        .statements
        .iter()
        .map(|statement| match &statement.kind {
            StatementKind::DefineReplica { leads, .. } => leads.clone(),
            other => panic!("{other:?}"),
        })
        .collect();
    assert!(
        matches!(&leads[0], Some(tessari_ql::ReachRef::Shard { table, shard: 2, .. }) if table.text == "orders"),
        "{leads:?}"
    );
    assert!(
        matches!(&leads[1], Some(tessari_ql::ReachRef::Namespace(name)) if name.text == "prod")
    );
    assert!(matches!(&leads[2], Some(tessari_ql::ReachRef::Database(_))));
    for refused in [
        "DEFINE REPLICA p AT 'b:9001' LEADS STORE;",
        "DEFINE REPLICA p AT 'b:9001' LEADS;",
    ] {
        let error = parse(refused).unwrap_err().to_string();
        assert!(error.contains("after `LEADS`"), "{refused}: {error}");
    }
    assert!(parse("DEFINE REPLICA p AT 'b:9001' LEADS SHARD prod.shop.orders 0;").is_err());
}
