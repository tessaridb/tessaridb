#![allow(clippy::unwrap_used)]

use tessari_session::Outcome;
use tessari_session::RunNow;
use tessari_storage::Now;
use tessari_storage::{Catalog, TableDefinition};
use tessari_types::{RecordId, Value};

use super::ShardSamples;
use crate::Db;

const DECLARED: &str =
    "DEFINE NAMESPACE prod; USE NAMESPACE prod; DEFINE DATABASE shop; USE DATABASE shop;";
const TENANCY: &str = "USE NAMESPACE prod; USE DATABASE shop;";

fn table(db: &Db) -> TableDefinition {
    table_named(db, "orders")
}

fn table_named(db: &Db, name: &str) -> TableDefinition {
    let mut reading = db.store().begin().now().unwrap();
    let catalog = Catalog::new(&mut reading);
    let namespace = catalog.namespace_id("prod").now().unwrap().unwrap();
    let database = catalog
        .database_id(namespace, "shop")
        .now()
        .unwrap()
        .unwrap();
    let id = catalog
        .table_id(namespace, database, name)
        .now()
        .unwrap()
        .unwrap();
    let found = catalog.table(id).now().unwrap().unwrap();
    reading.rollback();
    found
}

fn ids(db: &Db) -> Vec<RecordId> {
    let outcomes = db
        .session()
        .run_now(&format!("{TENANCY} SELECT * FROM orders;"))
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
        let pass = db.balance_shards(samples).now().unwrap();
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
        .run_now(&format!(
            "{DECLARED} DEFINE TABLE orders (n int) IDENTITY uuid SPREAD SPLIT AT '80'; \
             ALTER TABLE orders {policy};"
        ))
        .unwrap();
    let creates = "CREATE orders = { n: 1 };".repeat(records);
    db.session()
        .run_now(&format!("{TENANCY} {creates}"))
        .unwrap();
}

#[test]
fn a_table_too_big_for_its_shards_is_split_until_each_fits_and_keeps_every_record() {
    let db = Db::in_memory().now().unwrap();
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
    let db = Db::in_memory().now().unwrap();
    orders(
        &db,
        "SPLIT AUTOMATICALLY ABOVE 100 RECORDS MERGE BELOW 40 RECORDS",
        400,
    );
    passes(&db, &mut ShardSamples::default());
    let split = per_shard(&db).len();
    db.session()
        .run_now(&format!(
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
    let db = Db::in_memory().now().unwrap();
    orders(
        &db,
        "SPLIT AUTOMATICALLY ABOVE 100000 RECORDS OR 1 WRITES PER SECOND MERGE BELOW 2 RECORDS",
        0,
    );
    let mut samples = ShardSamples::default();
    // The first pass only learns where each shard's log stands.
    assert_eq!(db.balance_shards(&mut samples).now().unwrap().split, 0);
    let creates = "CREATE orders = { n: 1 };".repeat(40);
    db.session()
        .run_now(&format!("{TENANCY} {creates}"))
        .unwrap();
    let before = per_shard(&db).len();
    assert_eq!(db.balance_shards(&mut samples).now().unwrap().split, 1);
    assert_eq!(per_shard(&db).len(), before + 1);
    assert_eq!(ids(&db).len(), 40);
}

/// A table whose records sit evenly a01..a40 behind `SPLIT AT 'm'`, declared
/// with a load bound only the rate can reach. Identities are explicit uuid
/// v7-shaped values whose bytes sort at their position, so the key order is
/// the test's own; the split point above all forty leaves one shard empty.
fn hot_table(db: &Db, policy: &str) {
    let creates: String = (1..=40)
        .map(|n| format!("CREATE hot:uuid '{}' = {{ n: {n} }};", hot_id(n)))
        .collect();
    db.session()
        .run_now(&format!(
            "{DECLARED} DEFINE TABLE hot (n int) IDENTITY uuid \
             SPLIT AT uuid '{}'; {creates} ALTER TABLE hot {policy};",
            hot_id(250),
        ))
        .unwrap();
}

/// The identity text sorting at position `n`: fixed version and variant
/// fields, the rank in the trailing bytes.
fn hot_id(n: u8) -> String {
    format!("01800000-0000-7000-8000-{n:012x}")
}

/// Five writes into the hot low range, positions 1 through 5.
fn hot_writes(db: &Db) {
    let updates: String = (1..=5)
        .map(|n| format!("UPDATE hot:uuid '{}' SET n = 100;", hot_id(n)))
        .collect();
    db.session()
        .run_now(&format!("{TENANCY} {updates}"))
        .unwrap();
}

/// The writes a second each shard carried at the last published pass.
fn served_rates(db: &Db) -> Vec<u64> {
    let sampled = db.store().sampled_shards();
    let (_, table) = sampled
        .first()
        .unwrap_or_else(|| unreachable!("one table balances here"));
    table
        .shards
        .iter()
        .map(|shard| shard.writes_per_second.unwrap_or(0))
        .collect()
}

#[test]
fn a_hot_key_range_is_carved_out_by_the_traffic_median() {
    let db = Db::in_memory().now().unwrap();
    hot_table(
        &db,
        "SPLIT AUTOMATICALLY ABOVE 100000 RECORDS OR 1 WRITES PER SECOND \
         MERGE BELOW 2 RECORDS",
    );
    let mut samples = ShardSamples::default();
    // The first pass only learns where each log stands.
    assert_eq!(db.balance_shards(&mut samples).now().unwrap().split, 0);
    hot_writes(&db);
    assert_eq!(db.balance_shards(&mut samples).now().unwrap().split, 1);
    // The record median would have cut at position 21; the traffic median
    // of the five hot writes cuts at position 3, where the right half begins.
    let begins: Vec<String> = table_named(&db, "hot")
        .shards
        .unwrap()
        .spans()
        .filter_map(|span| span.from.map(|from| from.to_literal()))
        .collect();
    assert!(
        begins
            .iter()
            .any(|from| from == &format!("uuid '{}'", hot_id(3))),
        "the traffic median carved the hot range out: {begins:?}"
    );
}

#[test]
fn both_halves_serve_the_continued_traffic_within_the_stated_ratio() {
    let db = Db::in_memory().now().unwrap();
    hot_table(
        &db,
        "SPLIT AUTOMATICALLY ABOVE 100000 RECORDS OR 1 WRITES PER SECOND \
         MERGE BELOW 2 RECORDS",
    );
    let mut samples = ShardSamples::default();
    assert_eq!(db.balance_shards(&mut samples).now().unwrap().split, 0);
    hot_writes(&db);
    assert_eq!(db.balance_shards(&mut samples).now().unwrap().split, 1);
    // The same hot range keeps arriving: positions 1, 2 went left of the
    // cut, 3 began the right half, 4 and 5 follow it — 2 against 3.
    hot_writes(&db);
    let _ = db.balance_shards(&mut samples).now().unwrap();
    let served: Vec<u64> = served_rates(&db)
        .into_iter()
        .filter(|rate| *rate > 0)
        .collect();
    assert_eq!(
        served.len(),
        2,
        "the hot range reached both halves: {served:?}"
    );
    let hotter = served.iter().copied().max().unwrap_or(0);
    let cooler = served.iter().copied().min().unwrap_or(0);
    assert!(
        hotter.saturating_mul(2) <= cooler.saturating_mul(3),
        "the busier half serves at most 3/5 (the stated 60/40): {served:?}"
    );
}

#[test]
fn a_thin_sample_splits_at_the_record_median_as_before() {
    let db = Db::in_memory().now().unwrap();
    hot_table(
        &db,
        "SPLIT AUTOMATICALLY ABOVE 100000 RECORDS OR 1 WRITES PER SECOND \
         MERGE BELOW 2 RECORDS",
    );
    let mut samples = ShardSamples::default();
    assert_eq!(db.balance_shards(&mut samples).now().unwrap().split, 0);
    // One write is a sample too thin to name a mass: the point falls back to
    // the record median, and the whole hot range stays on one side of it.
    db.session()
        .run_now(&format!(
            "{TENANCY} UPDATE hot:uuid '{}' SET n = 100;",
            hot_id(1),
        ))
        .unwrap();
    assert_eq!(db.balance_shards(&mut samples).now().unwrap().split, 1);
    hot_writes(&db);
    let _ = db.balance_shards(&mut samples).now().unwrap();
    let served: Vec<u64> = served_rates(&db)
        .into_iter()
        .filter(|rate| *rate > 0)
        .collect();
    assert_eq!(
        served.len(),
        1,
        "the record median left the hot range on one shard: {served:?}"
    );
}

#[test]
fn a_table_split_manually_again_is_left_alone() {
    let db = Db::in_memory().now().unwrap();
    orders(
        &db,
        "SPLIT AUTOMATICALLY ABOVE 100 RECORDS MERGE BELOW 20 RECORDS",
        400,
    );
    db.session()
        .run_now(&format!("{TENANCY} ALTER TABLE orders SPLIT MANUALLY;"))
        .unwrap();
    assert_eq!(passes(&db, &mut ShardSamples::default()), (0, 0));
    assert_eq!(per_shard(&db).len(), 2);
}

#[test]
fn the_policy_is_refused_where_it_could_never_act_or_would_undo_itself() {
    let db = Db::in_memory().now().unwrap();
    db.session()
        .run_now(&format!(
            "{DECLARED} DEFINE TABLE plain (n int); DEFINE TABLE orders (n int) IDENTITY uuid SPLIT AT '80';"
        ))
        .unwrap();
    let refused = db.session().run_now(&format!(
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
    let refused = db.session().run_now(&format!(
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
    let refused = db.session().run_now(&format!(
        "{TENANCY} ALTER TABLE orders SPLIT AUTOMATICALLY ABOVE 100 RECORDS \
         MERGE BELOW 20 RECORDS MIN 4 SHARDS MAX 3 SHARDS;"
    ));
    assert!(
        matches!(
            refused,
            Err(tessari_session::Error::Store(
                tessari_storage::Error::AutoSplitMaxBelowMin {
                    min_shards: 4,
                    max_shards: 3,
                    ..
                }
            ))
        ),
        "{refused:?}"
    );
    // A shard count the grammar refuses: zero is no count at all.
    let refused = db.session().run_now(&format!(
        "{TENANCY} ALTER TABLE orders SPLIT AUTOMATICALLY ABOVE 100 RECORDS \
         MERGE BELOW 20 RECORDS MIN 0 SHARDS;"
    ));
    assert!(
        matches!(refused, Err(tessari_session::Error::Script(_))),
        "{refused:?}"
    );
}

#[test]
fn the_report_names_the_policy_and_its_definition_restores_it() {
    let db = Db::in_memory().now().unwrap();
    orders(
        &db,
        "SPLIT AUTOMATICALLY ABOVE 100 RECORDS OR 50 WRITES PER SECOND MERGE BELOW 20 RECORDS",
        0,
    );
    let outcomes = db
        .session()
        .run_now(&format!("{TENANCY} INFO FOR TABLE orders;"))
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
    let copy = Db::in_memory().now().unwrap();
    copy.session()
        .run_now(&format!("{DECLARED} {script}"))
        .unwrap();
    assert_eq!(table(&copy).auto_split, table(&db).auto_split);
}

#[test]
fn a_table_at_its_max_shard_count_is_not_split_further() {
    let db = Db::in_memory().now().unwrap();
    orders(
        &db,
        "SPLIT AUTOMATICALLY ABOVE 100 RECORDS MERGE BELOW 20 RECORDS MAX 3 SHARDS",
        400,
    );
    let mut samples = ShardSamples::default();
    let mut split = 0_usize;
    let mut refusal = None;
    for _ in 0..32 {
        let pass = db.balance_shards(&mut samples).now().unwrap();
        split = split.saturating_add(pass.split);
        refusal = pass.last_refusal.clone().or(refusal);
        if pass.split == 0 && pass.merged == 0 {
            break;
        }
    }
    // `SPLIT AT '80'` leaves two shards; one split is allowed, the next pass
    // meets the declaration's ceiling and refuses with nothing merged back.
    assert_eq!(split, 1, "one split under the ceiling");
    assert_eq!(per_shard(&db).len(), 3, "the ceiling held");
    assert!(
        refusal.as_ref().is_some_and(|why| why.contains("orders")),
        "{refusal:?}"
    );
    assert!(
        ids(&db).len() == 400,
        "a refused split moved no record and lost none"
    );
}

#[test]
fn a_table_at_its_min_shard_count_is_not_merged_further() {
    let db = Db::in_memory().now().unwrap();
    orders(
        &db,
        "SPLIT AUTOMATICALLY ABOVE 10000 RECORDS MERGE BELOW 40 RECORDS MIN 2 SHARDS",
        20,
    );
    let (_, merged) = passes(&db, &mut ShardSamples::default());
    assert_eq!(merged, 0, "the floor held");
    assert_eq!(per_shard(&db).len(), 2);
    // Without the floor the same quiet table merges its two near-empty
    // neighbours into one.
    let db = Db::in_memory().now().unwrap();
    orders(
        &db,
        "SPLIT AUTOMATICALLY ABOVE 10000 RECORDS MERGE BELOW 40 RECORDS",
        20,
    );
    let (_, merged) = passes(&db, &mut ShardSamples::default());
    assert!(merged >= 1);
    assert_eq!(per_shard(&db).len(), 1);
}

#[test]
fn a_policy_with_shard_count_bounds_round_trips_through_the_report_and_definition() {
    let db = Db::in_memory().now().unwrap();
    orders(
        &db,
        "SPLIT AUTOMATICALLY ABOVE 100 RECORDS MERGE BELOW 20 RECORDS \
         MIN 2 SHARDS MAX 8 SHARDS",
        0,
    );
    let outcomes = db
        .session()
        .run_now(&format!("{TENANCY} INFO FOR TABLE orders;"))
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
                    ("max_shards".to_owned(), Value::from(8_i64)),
                    ("merge_below".to_owned(), Value::from(20_i64)),
                    ("min_shards".to_owned(), Value::from(2_i64)),
                ]
                .into_iter()
                .collect()
            )
        ))
    );
    let Some(Value::String(script)) = fields.get("definition") else {
        unreachable!("a table reports its definition");
    };
    assert!(script.contains("MIN 2 SHARDS MAX 8 SHARDS"), "{script}");
    let copy = Db::in_memory().now().unwrap();
    copy.session()
        .run_now(&format!("{DECLARED} {script}"))
        .unwrap();
    assert_eq!(table(&copy).auto_split, table(&db).auto_split);
}

#[test]
fn what_a_pass_measured_is_reported_for_each_shard_with_its_last_act() {
    // ADR-0113 D4: the counts the pass took, not a second walk.
    let db = Db::in_memory().now().unwrap();
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
        .run_now(&format!("{TENANCY} INFO FOR TABLE orders;"))
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
    let outcomes = db.session().run_now("INFO FOR NODE;").unwrap();
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
