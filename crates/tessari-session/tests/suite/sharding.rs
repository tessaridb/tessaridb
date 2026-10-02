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

/// G034 S2.1 — a split table's record is written in its shard's log by a commit
/// touching one shard and in its database's by one touching two; its history is
/// both, newest first in the order they were committed, each event naming its
/// log. Merged by position instead, the database log's `at 1` would sort below
/// the shard log's later positions and the two-shard write would land out of
/// place.
#[test]
fn a_split_tables_record_history_merges_its_shard_and_database_logs_in_commit_order() {
    let store = store();
    let mut session = tenancy(&store);
    session
        .run(
            "DEFINE TABLE orders (total int) IDENTITY uuid SPLIT AT 'g'; \
             CREATE orders:'h' = { total: 1 }; CREATE orders:'a' = { total: 0 }; \
             UPDATE orders:'h' MERGE { total: 2 }; \
             BEGIN; UPDATE orders:'h' MERGE { total: 3 }; UPDATE orders:'a' MERGE { total: 9 }; COMMIT; \
             UPDATE orders:'h' MERGE { total: 4 };",
        )
        .unwrap();
    let Value::Object(history) = report(&mut session, "INFO FOR HISTORY OF orders:'h';") else {
        panic!("not a report");
    };
    let Some(Value::Array(events)) = history.get("events") else {
        panic!("no events: {history:?}");
    };
    let seen: Vec<(String, String)> = events
        .iter()
        .map(|event| {
            let Value::Object(event) = event else {
                panic!("not an event: {event:?}");
            };
            (
                format!(
                    "{:?}",
                    event.get("value").and_then(|value| match value {
                        Value::Object(fields) => fields.get("total").cloned(),
                        _ => None,
                    })
                ),
                format!("{:?}", event.get("log")),
            )
        })
        .collect();
    let expected: Vec<(String, String)> = [
        ("4", "shard 2"),
        ("3", "database"),
        ("2", "shard 2"),
        ("1", "shard 2"),
    ]
    .iter()
    .map(|(total, log)| {
        (
            format!("{:?}", Some(Value::from(total.parse::<i64>().unwrap()))),
            format!("{:?}", Some(Value::from(*log))),
        )
    })
    .collect();
    assert_eq!(seen, expected);
    assert_eq!(history.get("complete"), Some(&Value::Bool(true)));
    // The control: an unsplit table's history answers as it always did, with
    // no log named on its events.
    session
        .run("DEFINE TABLE plain (n int); CREATE plain:1 = { n: 1 };")
        .unwrap();
    let Value::Object(plain) = report(&mut session, "INFO FOR HISTORY OF plain:1;") else {
        panic!("not a report");
    };
    let Some(Value::Array(events)) = plain.get("events") else {
        panic!("no events: {plain:?}");
    };
    assert_eq!(events.len(), 1);
    assert!(!format!("{events:?}").contains("\"log\""), "{events:?}");
}

/// What `INFO FOR NODE` says a peer is subscribed to, by peer name.
fn subscribed(session: &mut Session<'_>) -> Vec<(String, Option<String>)> {
    peer_field(session, "replicates")
}

/// Every peer's name beside one text field of its `INFO FOR NODE` row.
fn peer_field(session: &mut Session<'_>, field: &str) -> Vec<(String, Option<String>)> {
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
            (text("name").unwrap_or_default(), text(field))
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

// ---- G032 S1.1: placement -------------------------------------------------

#[test]
fn a_peer_placed_to_lead_a_range_reports_it_in_the_clauses_spelling() {
    let store = store();
    let mut session = tenancy(&store);
    session
        .run(
            "DEFINE TABLE orders (total int) IDENTITY uuid SPLIT AT 'g', 'p'; \
             DEFINE REPLICA b AT 'b:9001' NODE '9f2c4e1a70bb43d5a1c6e2f480937d55' \
                 LEADS SHARD prod.shop.orders 2; \
             DEFINE REPLICA c AT 'c:9001' NODE '1f2c4e1a70bb43d5a1c6e2f480937d55' \
                 REPLICATES STORE LEADS DATABASE prod.shop; \
             DEFINE REPLICA d AT 'd:9001';",
        )
        .unwrap();
    assert_eq!(
        peer_field(&mut session, "leads"),
        vec![
            ("b".to_owned(), Some("SHARD prod.shop.orders 2".to_owned())),
            ("c".to_owned(), Some("DATABASE prod.shop".to_owned())),
            ("d".to_owned(), None),
        ]
    );
}

#[test]
fn a_peer_row_reports_where_clients_reach_it() {
    // ADR-0101: a redirect names these, so an operator must be able to read
    // back what a redirect will say — and a row that said nothing reads `null`,
    // which is the row whose redirects still name its peer door.
    let store = store();
    let mut session = tenancy(&store);
    session
        .run(
            "DEFINE REPLICA b AT 'b:9180' CLIENTS AT 'b.example:9080' \
                 HTTP AT 'https://b.example:8000'; \
             DEFINE REPLICA d AT 'd:9180';",
        )
        .unwrap();
    assert_eq!(
        peer_field(&mut session, "clients"),
        vec![
            ("b".to_owned(), Some("b.example:9080".to_owned())),
            ("d".to_owned(), None),
        ]
    );
    assert_eq!(
        peer_field(&mut session, "http"),
        vec![
            ("b".to_owned(), Some("https://b.example:8000".to_owned())),
            ("d".to_owned(), None),
        ]
    );
    let refused = session
        .run("DEFINE REPLICA e AT 'e:9180' CLIENTS 'e:9080';")
        .unwrap_err();
    assert!(
        refused
            .to_string()
            .contains("where a client reaches the peer"),
        "{refused}"
    );
}

#[test]
fn a_placement_on_a_shard_the_table_lacks_is_refused() {
    let store = store();
    let mut session = tenancy(&store);
    session
        .run("DEFINE TABLE orders (total int) IDENTITY uuid SPLIT AT 'g';")
        .unwrap();
    match session.run("DEFINE REPLICA b AT 'b:9001' LEADS SHARD prod.shop.orders 3;") {
        Err(Error::Unknown { entity, name, .. }) => {
            assert_eq!((entity, name.as_str()), ("shard", "prod.shop.orders 3"));
        }
        other => panic!("expected an unknown shard, got {other:?}"),
    }
    assert!(
        peer_field(&mut session, "leads").is_empty(),
        "nothing was declared"
    );
}

#[test]
fn a_row_that_places_a_leader_cannot_be_dropped_and_one_that_does_not_can() {
    // Dropping it would hand the range back to the store line on this node at
    // once, while the range's own leader keeps writing under its lease until
    // the drop reaches it (ADR-0082).
    let store = store();
    let mut session = tenancy(&store);
    session
        .run(
            "DEFINE TABLE orders (total int) IDENTITY uuid SPLIT AT 'g'; \
             DEFINE REPLICA b AT 'b:9001' LEADS SHARD prod.shop.orders 1; \
             DEFINE REPLICA d AT 'd:9001';",
        )
        .unwrap();
    let refused = refusal(&mut session, "DROP REPLICA b;");
    assert!(
        matches!(refused, tessari_storage::Error::PlacementCannotBeDropped { ref name } if name == "b"),
        "{refused:?}"
    );
    session.run("DROP REPLICA d;").unwrap();
    assert_eq!(
        peer_field(&mut session, "leads"),
        vec![("b".to_owned(), Some("SHARD prod.shop.orders 1".to_owned()))]
    );
}

// ---- G050 W-G050-6: a placement moves (ADR-0098) --------------------------------

#[test]
fn a_bound_peer_row_is_amended_in_place_and_keeps_its_node() {
    // Q-892: dropping a bound row tombstones its node (ADR-0108 D9), so a row
    // that moved or changed role is amended rather than declared again.
    let store = store();
    let mut session = tenancy(&store);
    // One transaction, judged against a catalog that names no peer yet: once a
    // row names a node this store is a cluster member and writes on a lease.
    session
        .run(
            "BEGIN; \
             DEFINE REPLICA b AT 'b:9001' NODE '9f2c4e1a70bb43d5a1c6e2f480937d55' \
             ROLES serving REPLICATES STORE; \
             ALTER REPLICA b AT 'b2:9001'; ALTER REPLICA b ROLES serving, writable, coordinating; \
             ALTER REPLICA b CLIENTS AT 'b2:9080'; ALTER REPLICA b HTTP AT 'http://b2:8000'; \
             ALTER REPLICA b HTTP NONE; \
             COMMIT;",
        )
        .unwrap();
    let field = |session: &mut Session<'_>, name: &str| peer_field(session, name)[0].1.clone();
    assert_eq!(field(&mut session, "endpoint").as_deref(), Some("b2:9001"));
    assert_eq!(field(&mut session, "clients").as_deref(), Some("b2:9080"));
    assert_eq!(field(&mut session, "http"), None);
    assert_eq!(field(&mut session, "replicates").as_deref(), Some("STORE"));
    let Value::Object(report) = report(&mut session, "INFO FOR NODE;") else {
        panic!("not a report");
    };
    let rendered = format!("{report:?}");
    let bound = Value::Uuid([
        0x9f, 0x2c, 0x4e, 0x1a, 0x70, 0xbb, 0x43, 0xd5, 0xa1, 0xc6, 0xe2, 0xf4, 0x80, 0x93, 0x7d,
        0x55,
    ]);
    assert!(
        rendered.contains("coordinating") && rendered.contains(&format!("{bound:?}")),
        "the roles changed and the node stayed bound: {rendered}"
    );

    for (refused, names) in [
        ("ALTER REPLICA nobody AT 'x:1';", "nobody"),
        ("ALTER REPLICA b ROLES flying;", "flying"),
    ] {
        let why = format!("{:?}", session.run(refused));
        assert!(
            why.starts_with("Err") && why.contains(names),
            "{refused}: {why}"
        );
    }
}

#[test]
fn a_placement_is_handed_to_another_row_and_taken_from_the_first() {
    let store = store();
    let mut session = tenancy(&store);
    session
        .run(
            "DEFINE TABLE orders (total int) IDENTITY uuid SPLIT AT 'g'; \
             DEFINE REPLICA a AT 'a:9001' LEADS SHARD prod.shop.orders 1; \
             DEFINE REPLICA b AT 'b:9001';",
        )
        .unwrap();
    session
        .run("ALTER REPLICA b LEADS SHARD prod.shop.orders 1; ALTER REPLICA a LEADS NONE;")
        .unwrap();
    assert_eq!(
        peer_field(&mut session, "leads"),
        vec![
            ("a".to_owned(), None),
            ("b".to_owned(), Some("SHARD prod.shop.orders 1".to_owned())),
        ]
    );
    // A row that is no longer the last candidate can be dropped.
    session
        .run("ALTER REPLICA a LEADS SHARD prod.shop.orders 1; DROP REPLICA b;")
        .unwrap();
    assert_eq!(
        peer_field(&mut session, "leads"),
        vec![("a".to_owned(), Some("SHARD prod.shop.orders 1".to_owned()))]
    );
}

#[test]
fn the_last_placement_on_a_range_is_not_taken_away() {
    let store = store();
    let mut session = tenancy(&store);
    session
        .run(
            "DEFINE TABLE orders (total int) IDENTITY uuid SPLIT AT 'g'; \
             DEFINE REPLICA a AT 'a:9001' LEADS SHARD prod.shop.orders 1;",
        )
        .unwrap();
    for taking in [
        "ALTER REPLICA a LEADS NONE;",
        "ALTER REPLICA a LEADS SHARD prod.shop.orders 2;",
    ] {
        let refused = refusal(&mut session, taking);
        assert!(
            matches!(refused, tessari_storage::Error::PlacementCannotBeDropped { ref name } if name == "a"),
            "{taking}: {refused:?}"
        );
    }
    match session.run("ALTER REPLICA z LEADS NONE;") {
        Err(Error::Unknown { entity, name, .. }) => {
            assert_eq!((entity, name.as_str()), ("replica", "z"));
        }
        other => panic!("expected an unknown replica, got {other:?}"),
    }
    match session.run("ALTER REPLICA a LEADS SHARD prod.shop.orders 3;") {
        Err(Error::Unknown { entity, .. }) => assert_eq!(entity, "shard"),
        other => panic!("expected an unknown shard, got {other:?}"),
    }
    assert_eq!(
        peer_field(&mut session, "leads"),
        vec![("a".to_owned(), Some("SHARD prod.shop.orders 1".to_owned()))]
    );
}

// ---- G050 W-G050-3: splitting and merging a table that already holds records --

fn orders(session: &mut Session<'_>) -> Vec<Reported> {
    shards_of(&report(session, "INFO FOR TABLE orders;")).unwrap()
}

/// A reported bound: the literal the clause takes back, quotes included.
fn text(value: &str) -> Option<String> {
    Some(format!("'{value}'"))
}

#[test]
fn alter_table_split_at_retires_the_shard_and_mints_two_in_its_place() {
    let store = store();
    let mut session = tenancy(&store);
    session
        .run("DEFINE TABLE orders (total int) IDENTITY uuid SPLIT AT 'g', 'p';")
        .unwrap();
    session.run("ALTER TABLE orders SPLIT AT 'm';").unwrap();
    assert_eq!(
        orders(&mut session),
        vec![
            (1, None, text("g")),
            (4, text("g"), text("m")),
            (5, text("m"), text("p")),
            (3, text("p"), None),
        ]
    );
}

#[test]
fn a_moved_map_reports_its_version_and_what_it_retired() {
    let store = store();
    let mut session = tenancy(&store);
    session
        .run("DEFINE TABLE orders (total int) IDENTITY uuid SPLIT AT 'g', 'p';")
        .unwrap();
    let declared = report(&mut session, "INFO FOR TABLE orders;");
    let field = |report: &Value, name: &str| match report {
        Value::Object(fields) => fields.get(name).cloned(),
        other => panic!("a report is an object, got {other:?}"),
    };
    assert_eq!(field(&declared, "version"), Some(Value::from(0_i64)));
    assert_eq!(field(&declared, "retired"), Some(Value::Array(Vec::new())));
    session.run("ALTER TABLE orders SPLIT AT 'm';").unwrap();
    let moved = report(&mut session, "INFO FOR TABLE orders;");
    assert_eq!(field(&moved, "version"), Some(Value::from(1_i64)));
    let retired = Value::Object(
        [
            ("id".to_owned(), Value::from(2_i64)),
            (
                "into".to_owned(),
                Value::Array(vec![Value::from(4_i64), Value::from(5_i64)]),
            ),
        ]
        .into_iter()
        .collect(),
    );
    assert_eq!(field(&moved, "retired"), Some(Value::Array(vec![retired])));
}

#[test]
fn alter_table_merge_shard_joins_two_neighbours_into_one_new_shard() {
    let store = store();
    let mut session = tenancy(&store);
    session
        .run("DEFINE TABLE orders (total int) IDENTITY uuid SPLIT AT 'g', 'p';")
        .unwrap();
    session.run("ALTER TABLE orders MERGE SHARD 3, 2;").unwrap();
    assert_eq!(
        orders(&mut session),
        vec![(1, None, text("g")), (4, text("g"), None)]
    );
}

#[test]
fn every_record_reads_back_after_a_split_and_a_merge_moved_the_map() {
    let store = store();
    let mut session = tenancy(&store);
    session
        .run(
            "DEFINE TABLE orders (total int) IDENTITY uuid SPLIT AT 'g', 'p'; \
             CREATE orders:'h' = { total: 1 }; CREATE orders:'n' = { total: 2 };",
        )
        .unwrap();
    session.run("ALTER TABLE orders SPLIT AT 'm';").unwrap();
    session
        .run("CREATE orders:'k' = { total: 3 }; CREATE orders:'o' = { total: 4 };")
        .unwrap();
    session.run("ALTER TABLE orders MERGE SHARD 4, 5;").unwrap();
    session.run("CREATE orders:'j' = { total: 5 };").unwrap();
    // `h` 1, `j` 5, `k` 3, `n` 2, `o` 4 — written before, between and after the
    // two changes, and every one answered in identity order.
    let Some(Outcome::Records { records, .. }) = session
        .run("SELECT total FROM orders ORDER BY id;")
        .unwrap()
        .last()
        .cloned()
    else {
        panic!("a read answers records");
    };
    let read: Vec<(String, Value)> = records
        .into_iter()
        .map(|(id, row)| {
            let tessari_types::RecordId::Text(id) = id else {
                panic!("the ids here are text, got {id:?}");
            };
            let Value::Object(fields) = row else {
                panic!("a row is an object, got {row:?}");
            };
            (id, fields.get("total").cloned().unwrap_or(Value::None))
        })
        .collect();
    let expected: Vec<(String, Value)> = [("h", 1_i64), ("j", 5), ("k", 3), ("n", 2), ("o", 4)]
        .into_iter()
        .map(|(id, total)| (id.to_owned(), Value::from(total)))
        .collect();
    assert_eq!(read, expected);
}

#[test]
fn a_split_of_a_table_that_is_not_split_is_refused_by_name() {
    let store = store();
    let mut session = tenancy(&store);
    session.run("DEFINE TABLE notes (body string);").unwrap();
    assert!(matches!(
        refusal(&mut session, "ALTER TABLE notes SPLIT AT 'm';"),
        tessari_storage::Error::NotASplitTable { .. }
    ));
}
