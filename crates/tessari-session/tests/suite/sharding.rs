//! A table split by key range — G031, ADR-0080.
//!
//! S1.1 and S1.2 here: the declaration, what the catalog keeps of it, what
//! `INFO FOR TABLE` reports, and the refusals. Each refusal test names the error
//! it expects, because a test that any failure satisfies would stay green while
//! an upstream change re-pointed it at a different refusal.

#![allow(clippy::panic, clippy::unwrap_used)]

use std::sync::Arc;

use tessari_kv::{KvBackend, MemoryBackend};
use tessari_session::{Error, Outcome, Session};
use tessari_storage::Store;
use tessari_types::Value;

fn store() -> Store {
    Store::open(Arc::new(MemoryBackend::new()) as Arc<dyn KvBackend>).unwrap()
}

fn tenancy(store: &Store) -> Session<'_> {
    let mut session = Session::new(store);
    session
        .run("DEFINE NAMESPACE prod; USE NAMESPACE prod; DEFINE DATABASE shop; USE DATABASE shop;")
        .unwrap();
    session
}

fn report(session: &mut Session<'_>, script: &str) -> Value {
    match session.run(script).unwrap().last() {
        Some(Outcome::Value(value)) => value.clone(),
        other => panic!("expected a report, got {other:?}"),
    }
}

/// One reported shard: its id and the literals at both ends, `None` when open.
type Reported = (i64, Option<String>, Option<String>);

/// Every shard out of a table report, or `None` when the table is not split.
fn shards_of(report: &Value) -> Option<Vec<Reported>> {
    let Value::Object(fields) = report else {
        panic!("expected an object, got {report:?}");
    };
    let Value::Array(shards) = fields.get("shards")? else {
        panic!("shards is not a list in {report:?}");
    };
    let end = |value: Option<&Value>| match value {
        Some(Value::String(literal)) => Some(literal.clone()),
        Some(Value::None) | None => None,
        Some(other) => panic!("a shard bound is reported as its literal, got {other:?}"),
    };
    Some(
        shards
            .iter()
            .map(|shard| {
                let Value::Object(shard) = shard else {
                    panic!("a shard is an object, got {shard:?}");
                };
                let Some(Value::Number(id)) = shard.get("id") else {
                    panic!("a shard carries its id: {shard:?}");
                };
                (
                    id.as_exact_integer().unwrap(),
                    end(shard.get("from")),
                    end(shard.get("to")),
                )
            })
            .collect(),
    )
}

fn refusal(session: &mut Session<'_>, script: &str) -> tessari_storage::Error {
    match session.run(script) {
        Err(Error::Store(refused)) => refused,
        other => panic!("{script}\n  expected a store refusal, got {other:?}"),
    }
}

#[test]
fn a_split_table_reports_its_shards_in_key_order_with_both_ends() {
    let store = store();
    let mut session = tenancy(&store);
    session
        .run("DEFINE TABLE orders (total int) IDENTITY uuid SPLIT AT 'g', 'p';")
        .unwrap();
    let described = report(&mut session, "INFO FOR TABLE orders;");
    assert_eq!(
        shards_of(&described).expect("a split table reports its shards"),
        vec![
            (1, None, Some("'g'".to_owned())),
            (2, Some("'g'".to_owned()), Some("'p'".to_owned())),
            (3, Some("'p'".to_owned()), None),
        ]
    );
}

#[test]
fn a_reported_bound_is_the_literal_the_clause_takes_back() {
    // What `INFO` prints is what the next declaration types: re-declaring a
    // table from the report lands on the same map.
    let store = store();
    let mut session = tenancy(&store);
    session
        .run(
            "DEFINE TABLE events (n int) IDENTITY uuid \
             SPLIT AT uuid '0195e0a1-7c2e-7000-8000-000000000000';",
        )
        .unwrap();
    let first = shards_of(&report(&mut session, "INFO FOR TABLE events;")).unwrap();
    let bound = first[0].2.clone().unwrap();
    session
        .run(&format!(
            "DEFINE TABLE again (n int) IDENTITY uuid SPLIT AT {bound};"
        ))
        .unwrap();
    let second = shards_of(&report(&mut session, "INFO FOR TABLE again;")).unwrap();
    assert_eq!(first, second);
}

#[test]
fn a_table_that_is_not_split_reports_no_shards() {
    let store = store();
    let mut session = tenancy(&store);
    session.run("DEFINE TABLE plain (n int);").unwrap();
    assert_eq!(
        shards_of(&report(&mut session, "INFO FOR TABLE plain;")),
        None
    );
}

#[test]
fn a_counter_identity_cannot_be_split() {
    let store = store();
    let mut session = tenancy(&store);
    let refused = refusal(
        &mut session,
        "DEFINE TABLE orders (total int) SPLIT AT 'g';",
    );
    assert!(
        matches!(refused, tessari_storage::Error::SplitNeedsGeneratedUuid { ref table } if table == "orders"),
        "{refused:?}"
    );
    // And nothing was left behind: the refusal took the whole statement.
    assert!(session.run("INFO FOR TABLE orders;").is_err());
}

#[test]
fn points_out_of_order_or_repeated_are_refused_by_position() {
    let store = store();
    let mut session = tenancy(&store);
    let refused = refusal(
        &mut session,
        "DEFINE TABLE a (n int) IDENTITY uuid SPLIT AT 'p', 'g';",
    );
    assert!(
        matches!(
            refused,
            tessari_storage::Error::SplitPointsOutOfOrder { position: 2, .. }
        ),
        "{refused:?}"
    );
    let refused = refusal(
        &mut session,
        "DEFINE TABLE b (n int) IDENTITY uuid SPLIT AT 'g', 'g';",
    );
    assert!(
        matches!(
            refused,
            tessari_storage::Error::SplitPointsOutOfOrder { position: 2, .. }
        ),
        "{refused:?}"
    );
}

#[test]
fn a_kind_that_is_not_records_cannot_be_split() {
    let store = store();
    let mut session = tenancy(&store);
    let refused = refusal(
        &mut session,
        "DEFINE TABLE follows EDGE IDENTITY uuid SPLIT AT 'g';",
    );
    assert!(
        matches!(
            refused,
            tessari_storage::Error::SplitOnAKindThatIsNotRecords {
                kind: "an edge table",
                ..
            }
        ),
        "{refused:?}"
    );
}

#[test]
fn the_definition_a_report_carries_re_creates_the_same_shards() {
    // `INFO FOR TABLE` also hands back a script that re-creates the table. A
    // script that dropped `SPLIT AT` would restore an unsplit table and nothing
    // anywhere would report the loss, so the round trip is asserted on the map
    // the re-created table ends up with, not on the text.
    let store = store();
    let mut session = tenancy(&store);
    session
        .run("DEFINE TABLE orders (total int) IDENTITY uuid SPLIT AT 'g', 'p';")
        .unwrap();
    let described = report(&mut session, "INFO FOR TABLE orders;");
    let Value::Object(fields) = &described else {
        panic!("expected an object");
    };
    let Some(Value::String(script)) = fields.get("definition") else {
        panic!("a split table is definable: {described:?}");
    };
    let restored = store_with(&script.replace("orders", "restored"));
    let mut reader = Session::new(&restored);
    reader
        .run("USE NAMESPACE prod; USE DATABASE shop;")
        .unwrap();
    assert_eq!(
        shards_of(&report(&mut reader, "INFO FOR TABLE restored;")),
        shards_of(&described),
        "the script restored a different map: {script}"
    );
}

/// A fresh store holding the tenancy and whatever `script` declares in it.
fn store_with(script: &str) -> Store {
    let fresh = store();
    let mut session = tenancy(&fresh);
    session.run(script).unwrap();
    fresh
}

#[test]
fn a_split_tables_record_history_is_refused_rather_than_answered_from_one_log() {
    // Its commits are in its shards' logs, and a commit spanning two shards is
    // in its database's: a history read from any one of them is missing what
    // the others hold, and would still read as a history.
    let store = store();
    let mut session = tenancy(&store);
    session
        .run(
            "DEFINE TABLE orders (total int) IDENTITY uuid SPLIT AT 'g'; \
             CREATE orders:'h' = { total: 1 };",
        )
        .unwrap();
    match session.run("INFO FOR HISTORY OF orders:'h';") {
        Err(Error::SpansShardLogs { table, .. }) => assert_eq!(table, "orders"),
        other => panic!("expected SpansShardLogs, got {other:?}"),
    }
    // The control: an unsplit table's history still answers.
    session
        .run("DEFINE TABLE plain (n int); CREATE plain:1 = { n: 1 };")
        .unwrap();
    assert!(session.run("INFO FOR HISTORY OF plain:1;").is_ok());
}

/// What `INFO FOR NODE` says a peer is subscribed to, by peer name.
fn subscribed(session: &mut Session<'_>) -> Vec<(String, Option<String>)> {
    let Value::Object(report) = report(session, "INFO FOR NODE;") else {
        panic!("not a report");
    };
    let Some(Value::Object(cluster)) = report.get("cluster") else {
        panic!("no cluster group: {report:?}");
    };
    let Some(Value::Array(peers)) = cluster.get("peers") else {
        panic!("no peer list: {cluster:?}");
    };
    peers
        .iter()
        .map(|peer| {
            let Value::Object(fields) = peer else {
                panic!("a peer is an object");
            };
            let text = |key: &str| match fields.get(key) {
                Some(Value::String(text)) => Some(text.clone()),
                _ => None,
            };
            (text("name").unwrap_or_default(), text("replicates"))
        })
        .collect()
}

#[test]
fn a_peer_subscribes_to_one_shard_and_the_report_spells_it_back() {
    let store = store();
    let mut session = tenancy(&store);
    session
        .run(
            "DEFINE TABLE orders (total int) IDENTITY uuid SPLIT AT 'g', 'p'; \
             DEFINE REPLICA part AT 'b:9001' NODE '9f2c4e1a70bb43d5a1c6e2f480937d55' \
                 REPLICATES SHARD prod.shop.orders 2;",
        )
        .unwrap();
    assert_eq!(
        subscribed(&mut session),
        vec![(
            "part".to_owned(),
            Some("SHARD prod.shop.orders 2".to_owned())
        )]
    );
}

#[test]
fn a_subscription_to_a_shard_that_does_not_exist_is_refused() {
    // A subscription to nothing is the quietest failure a cluster has: every
    // node up, every greeting landing, one copy that never changes.
    let store = store();
    let mut session = tenancy(&store);
    session
        .run("DEFINE TABLE orders (total int) IDENTITY uuid SPLIT AT 'g'; DEFINE TABLE plain (n int);")
        .unwrap();
    for (clause, shown) in [
        ("SHARD prod.shop.orders 3", "prod.shop.orders 3"),
        ("SHARD prod.shop.plain 1", "prod.shop.plain 1"),
        ("SHARD prod.shop.nowhere 1", "prod.shop.nowhere 1"),
    ] {
        match session.run(&format!(
            "DEFINE REPLICA r AT 'b:9001' NODE '9f2c4e1a70bb43d5a1c6e2f480937d55' REPLICATES {clause};"
        )) {
            Err(Error::Unknown { entity, name, .. }) => {
                assert_eq!((entity, name.as_str()), ("shard", shown));
            }
            other => panic!("{clause}: expected an unknown shard, got {other:?}"),
        }
    }
}

#[test]
fn a_graphs_node_table_cannot_be_split() {
    // A walk reaches a node from its neighbours, and there is no span to confine
    // it to the shards a node holds — so a node holding part of one would answer
    // every traversal from the part.
    let store = store();
    let mut session = tenancy(&store);
    session.run("DEFINE GRAPH social;").unwrap();
    let refused = refusal(
        &mut session,
        "DEFINE TABLE person (n int) IN social IDENTITY uuid SPLIT AT 'g';",
    );
    assert!(
        matches!(
            refused,
            tessari_storage::Error::SplitOnAKindThatIsNotRecords {
                kind: "a node table of a graph",
                ..
            }
        ),
        "{refused:?}"
    );
}
