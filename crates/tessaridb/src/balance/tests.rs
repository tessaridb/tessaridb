#![allow(clippy::unwrap_used)]

use tessari_session::Outcome;
use tessari_storage::{Catalog, TableDefinition};
use tessari_types::{RecordId, Value};

use super::ShardSamples;
use crate::Db;

const DECLARED: &str =
    "DEFINE NAMESPACE prod; USE NAMESPACE prod; DEFINE DATABASE shop; USE DATABASE shop;";
const TENANCY: &str = "USE NAMESPACE prod; USE DATABASE shop;";

fn table(db: &Db) -> TableDefinition {
    let mut reading = db.store().begin().unwrap();
    let catalog = Catalog::new(&mut reading);
    let namespace = catalog.namespace_id("prod").unwrap().unwrap();
    let database = catalog.database_id(namespace, "shop").unwrap().unwrap();
    let id = catalog
        .table_id(namespace, database, "orders")
        .unwrap()
        .unwrap();
    let found = catalog.table(id).unwrap().unwrap();
    reading.rollback();
    found
}

fn ids(db: &Db) -> Vec<RecordId> {
    let outcomes = db
        .session()
        .run(&format!("{TENANCY} SELECT * FROM orders;"))
        .unwrap();
    match outcomes.last() {
        Some(Outcome::Records { records, .. }) => {
            records.iter().map(|(id, _)| id.clone()).collect()
        }
        _ => Vec::new(),
    }
}

/// How many records each live shard holds, by the table's map now.
fn per_shard(db: &Db) -> Vec<usize> {
    let map = table(db).shards.unwrap();
    let held = ids(db);
    map.spans()
        .map(|span| held.iter().filter(|id| map.shard_of(id) == span.id).count())
        .collect()
}

fn passes(db: &Db, samples: &mut ShardSamples) -> (usize, usize) {
    let (mut split, mut merged) = (0_usize, 0_usize);
    for _ in 0..32 {
        let pass = db.balance_shards(samples).unwrap();
        assert_eq!(pass.last_refusal, None);
        if pass.split == 0 && pass.merged == 0 {
            break;
        }
        split = split.saturating_add(pass.split);
        merged = merged.saturating_add(pass.merged);
    }
    (split, merged)
}

fn orders(db: &Db, policy: &str, records: usize) {
    db.session()
        .run(&format!(
            "{DECLARED} DEFINE TABLE orders (n int) IDENTITY uuid SPREAD SPLIT AT '80'; \
             ALTER TABLE orders {policy};"
        ))
        .unwrap();
    let creates = "CREATE orders = { n: 1 };".repeat(records);
    db.session().run(&format!("{TENANCY} {creates}")).unwrap();
}

#[test]
fn a_table_too_big_for_its_shards_is_split_until_each_fits_and_keeps_every_record() {
    let db = Db::in_memory().unwrap();
    orders(
        &db,
        "SPLIT AUTOMATICALLY ABOVE 100 RECORDS MERGE BELOW 20 RECORDS",
        400,
    );
    let before = ids(&db);
    let (split, merged) = passes(&db, &mut ShardSamples::default());
    assert!(split >= 2, "{split} splits");
    assert_eq!(merged, 0);
    let shards = per_shard(&db);
    assert!(shards.iter().all(|held| *held <= 100), "{shards:?}");
    let mut after = ids(&db);
    let mut expected = before;
    after.sort();
    expected.sort();
    assert_eq!(after, expected, "a split moved no record and lost none");
}

#[test]
fn neighbours_emptied_below_the_bound_are_merged_back() {
    let db = Db::in_memory().unwrap();
    orders(
        &db,
        "SPLIT AUTOMATICALLY ABOVE 100 RECORDS MERGE BELOW 40 RECORDS",
        400,
    );
    passes(&db, &mut ShardSamples::default());
    let split = per_shard(&db).len();
    db.session()
        .run(&format!(
            "{TENANCY} DELETE FROM orders WHERE n = 1 LIMIT 390;"
        ))
        .unwrap();
    let (_, merged) = passes(&db, &mut ShardSamples::default());
    assert!(merged >= 1);
    assert!(per_shard(&db).len() < split);
    assert_eq!(ids(&db).len(), 10);
}

#[test]
fn a_busy_shard_is_split_however_small() {
    let db = Db::in_memory().unwrap();
    orders(
        &db,
        "SPLIT AUTOMATICALLY ABOVE 100000 RECORDS OR 1 WRITES PER SECOND MERGE BELOW 2 RECORDS",
        0,
    );
    let mut samples = ShardSamples::default();
    // The first pass only learns where each shard's log stands.
    assert_eq!(db.balance_shards(&mut samples).unwrap().split, 0);
    let creates = "CREATE orders = { n: 1 };".repeat(40);
    db.session().run(&format!("{TENANCY} {creates}")).unwrap();
    let before = per_shard(&db).len();
    assert_eq!(db.balance_shards(&mut samples).unwrap().split, 1);
    assert_eq!(per_shard(&db).len(), before + 1);
    assert_eq!(ids(&db).len(), 40);
}

#[test]
fn a_table_split_manually_again_is_left_alone() {
    let db = Db::in_memory().unwrap();
    orders(
        &db,
        "SPLIT AUTOMATICALLY ABOVE 100 RECORDS MERGE BELOW 20 RECORDS",
        400,
    );
    db.session()
        .run(&format!("{TENANCY} ALTER TABLE orders SPLIT MANUALLY;"))
        .unwrap();
    assert_eq!(passes(&db, &mut ShardSamples::default()), (0, 0));
    assert_eq!(per_shard(&db).len(), 2);
}

#[test]
fn the_policy_is_refused_where_it_could_never_act_or_would_undo_itself() {
    let db = Db::in_memory().unwrap();
    db.session()
        .run(&format!(
            "{DECLARED} DEFINE TABLE plain (n int); DEFINE TABLE orders (n int) IDENTITY uuid SPLIT AT '80';"
        ))
        .unwrap();
    let refused = db.session().run(&format!(
        "{TENANCY} ALTER TABLE plain SPLIT AUTOMATICALLY ABOVE 100 RECORDS MERGE BELOW 20 RECORDS;"
    ));
    assert!(
        matches!(
            refused,
            Err(tessari_session::Error::Store(
                tessari_storage::Error::AutoSplitOnAnUnsplitTable { .. }
            ))
        ),
        "{refused:?}"
    );
    let refused = db.session().run(&format!(
        "{TENANCY} ALTER TABLE orders SPLIT AUTOMATICALLY ABOVE 100 RECORDS MERGE BELOW 50 RECORDS;"
    ));
    assert!(
        matches!(
            refused,
            Err(tessari_session::Error::Store(
                tessari_storage::Error::AutoSplitWouldOscillate {
                    above: 100,
                    merge_below: 50,
                    ..
                }
            ))
        ),
        "{refused:?}"
    );
}

#[test]
fn the_report_names_the_policy_and_its_definition_restores_it() {
    let db = Db::in_memory().unwrap();
    orders(
        &db,
        "SPLIT AUTOMATICALLY ABOVE 100 RECORDS OR 50 WRITES PER SECOND MERGE BELOW 20 RECORDS",
        0,
    );
    let outcomes = db
        .session()
        .run(&format!("{TENANCY} INFO FOR TABLE orders;"))
        .unwrap();
    let Some(Outcome::Value(Value::Object(fields))) = outcomes.last() else {
        unreachable!("INFO FOR TABLE answers a value");
    };
    let policy = fields.get("auto_split").cloned();
    assert_eq!(
        policy.map(|held| format!("{held:?}")),
        Some(format!(
            "{:?}",
            Value::Object(
                [
                    ("above".to_owned(), Value::from(100_i64)),
                    ("merge_below".to_owned(), Value::from(20_i64)),
                    ("writes_per_second".to_owned(), Value::from(50_i64)),
                ]
                .into_iter()
                .collect()
            )
        ))
    );
    let Some(Value::String(script)) = fields.get("definition") else {
        unreachable!("a table reports its definition");
    };
    let copy = Db::in_memory().unwrap();
    copy.session().run(&format!("{DECLARED} {script}")).unwrap();
    assert_eq!(table(&copy).auto_split, table(&db).auto_split);
}

#[test]
fn what_a_pass_measured_is_reported_for_each_shard_with_its_last_act() {
    // ADR-0113 D4: the counts the pass took, not a second walk.
    let db = Db::in_memory().unwrap();
    orders(
        &db,
        "SPLIT AUTOMATICALLY ABOVE 100 RECORDS MERGE BELOW 20 RECORDS",
        400,
    );
    passes(&db, &mut ShardSamples::default());
    let sampled = db.store().sampled_shards();
    assert_eq!(sampled.len(), 1);
    let (_, table) = &sampled[0];
    assert_eq!(table.name, "prod.shop.orders");
    assert_eq!(table.shards.len(), per_shard(&db).len());
    assert!(table.shards.iter().all(|shard| shard.complete));
    let counted: Vec<usize> = table
        .shards
        .iter()
        .map(|shard| usize::try_from(shard.records).unwrap())
        .collect();
    assert_eq!(
        counted,
        per_shard(&db),
        "the sample is the shards' own count"
    );
    assert!(
        table
            .last_act
            .as_deref()
            .is_some_and(|act| act.starts_with("split at ")),
        "{:?}",
        table.last_act
    );
    let outcomes = db
        .session()
        .run(&format!("{TENANCY} INFO FOR TABLE orders;"))
        .unwrap();
    let Some(Outcome::Value(Value::Object(fields))) = outcomes.last() else {
        unreachable!("INFO FOR TABLE answers a value");
    };
    let Some(Value::Object(reported)) = fields.get("sampled") else {
        unreachable!("no sample reported: {fields:?}");
    };
    let Some(Value::Array(shards)) = reported.get("shards") else {
        unreachable!("no shards in the sample: {reported:?}");
    };
    assert_eq!(shards.len(), counted.len());
    // And the node's own report lists it, for the console's cluster map.
    let outcomes = db.session().run("INFO FOR NODE;").unwrap();
    let Some(Outcome::Value(Value::Object(node))) = outcomes.last() else {
        unreachable!("INFO FOR NODE answers a value");
    };
    let Some(Value::Object(cluster)) = node.get("cluster") else {
        unreachable!("no cluster group: {node:?}");
    };
    let Some(Value::Array(balanced)) = cluster.get("balanced") else {
        unreachable!("no balanced tables: {cluster:?}");
    };
    assert_eq!(balanced.len(), 1);
    assert!(
        format!("{balanced:?}").contains("prod.shop.orders"),
        "{balanced:?}"
    );
}
