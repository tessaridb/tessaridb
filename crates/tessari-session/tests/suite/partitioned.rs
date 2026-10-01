//! A table partitioned by a field — G050 W-G050-5, ADR-0096.
//!
//! The partition field is the leading part of every record's identity, so a
//! region's records sit together and a split at a region's name keeps them in
//! one shard. Each refusal test names the refusal it expects.

#![allow(clippy::panic, clippy::unwrap_used)]

use std::sync::Arc;

use tessari_kv::{KvBackend, MemoryBackend};
use tessari_session::{Error, Outcome, Session};
use tessari_storage::Store;
use tessari_types::{RecordId, Value};

fn store() -> Store {
    Store::open(Arc::new(MemoryBackend::new()) as Arc<dyn KvBackend>).unwrap()
}

/// A tenancy holding `customers`, partitioned by `region` and split at 'de', 'fr'.
fn customers(store: &Store) -> Session<'_> {
    let mut session = Session::new(store);
    session
        .run(
            "DEFINE NAMESPACE prod; USE NAMESPACE prod; DEFINE DATABASE shop; USE DATABASE shop;\n\
             DEFINE TABLE customers (region string, name string) IDENTITY uuid \
             PARTITION BY region SPLIT AT 'de', 'fr';",
        )
        .unwrap();
    session
}

fn ids(session: &mut Session<'_>, read: &str) -> Vec<RecordId> {
    match session.run(read).unwrap().last() {
        Some(Outcome::Records { records, .. }) => {
            records.iter().map(|(id, _)| id.clone()).collect()
        }
        other => panic!("{read}: {other:?}"),
    }
}

fn refusal(session: &mut Session<'_>, script: &str) -> tessari_storage::Error {
    match session.run(script) {
        Err(Error::Store(refused)) => refused,
        other => panic!("{script}\n  expected a store refusal, got {other:?}"),
    }
}

fn mismatch(refused: tessari_storage::Error) -> (String, String) {
    match refused {
        tessari_storage::Error::PartitionMismatch { record, field, .. } => {
            (record.to_string(), field.to_string())
        }
        other => panic!("expected PartitionMismatch, got {other:?}"),
    }
}

#[test]
fn a_record_the_store_names_carries_its_region_before_the_rest() {
    let store = store();
    let mut session = customers(&store);
    session
        .run("CREATE customers = { region: 'de', name: 'ada' }; CREATE customers = { region: 'fr', name: 'bo' };")
        .unwrap();
    let named = ids(&mut session, "SELECT * FROM customers;");
    assert_eq!(named.len(), 2, "{named:?}");
    let prefixes: Vec<String> = named
        .iter()
        .map(|id| match id {
            RecordId::Text(text) => text.split_once(':').unwrap().0.to_owned(),
            other => panic!("a partitioned record is named by text, got {other:?}"),
        })
        .collect();
    assert_eq!(prefixes, vec!["de".to_owned(), "fr".to_owned()]);
}

#[test]
fn a_write_whose_identity_and_region_disagree_is_refused_by_name() {
    let store = store();
    let mut session = customers(&store);
    session
        .run("CREATE customers:'de:1' = { region: 'de', name: 'ada' };")
        .unwrap();
    assert_eq!(
        mismatch(refusal(
            &mut session,
            "CREATE customers:'fr:2' = { region: 'de', name: 'bo' };"
        )),
        ("fr:2".to_owned(), "region".to_owned())
    );
    // Moving a record between regions is a delete and a create, never an update.
    assert_eq!(
        mismatch(refusal(
            &mut session,
            "UPDATE customers:'de:1' SET region = 'fr';"
        ))
        .1,
        "region"
    );
    // A region holding the separator would put its records inside another's span.
    assert_eq!(
        mismatch(refusal(
            &mut session,
            "CREATE customers:'de:x:3' = { region: 'de:x', name: 'cy' };"
        ))
        .1,
        "region"
    );
    // Absent, the region names nothing for the identity to begin with.
    assert_eq!(
        mismatch(refusal(
            &mut session,
            "CREATE customers:'de:4' = { name: 'di' };"
        ))
        .1,
        "region"
    );
    assert_eq!(ids(&mut session, "SELECT * FROM customers;").len(), 1);
}

#[test]
fn a_partition_needs_the_store_to_name_the_rest_of_the_identity() {
    let store = store();
    let mut session = Session::new(&store);
    session
        .run("DEFINE NAMESPACE prod; USE NAMESPACE prod; DEFINE DATABASE shop; USE DATABASE shop;")
        .unwrap();
    match refusal(
        &mut session,
        "DEFINE TABLE customers (region string) PARTITION BY region;",
    ) {
        tessari_storage::Error::PartitionNeedsGeneratedUuid { table } => {
            assert_eq!(table.as_str(), "customers");
        }
        other => panic!("expected PartitionNeedsGeneratedUuid, got {other:?}"),
    }
}

#[test]
fn the_report_names_the_partition_and_its_definition_re_creates_it() {
    let store = store();
    let mut session = customers(&store);
    let described = match session.run("INFO FOR TABLE customers;").unwrap().last() {
        Some(Outcome::Value(value)) => value.clone(),
        other => panic!("{other:?}"),
    };
    let Value::Object(fields) = &described else {
        panic!("{described:?}");
    };
    assert_eq!(fields.get("partition"), Some(&Value::from("region")));
    let Some(Value::String(script)) = fields.get("definition") else {
        panic!("{described:?}");
    };
    let restored = Store::open(Arc::new(MemoryBackend::new()) as Arc<dyn KvBackend>).unwrap();
    let mut again = Session::new(&restored);
    again
        .run(&format!(
            "DEFINE NAMESPACE prod; USE NAMESPACE prod; DEFINE DATABASE shop; USE DATABASE shop; {script}"
        ))
        .unwrap();
    assert_eq!(
        mismatch(refusal(
            &mut again,
            "CREATE customers:'fr:1' = { region: 'de', name: 'x' };"
        ))
        .1,
        "region",
        "the restored table is not partitioned: {script}"
    );
}

/// The plan a value reports, as `access` and the shards it names.
fn access_and_shards(plan: &Value) -> (String, Option<Vec<i64>>) {
    let Value::Object(fields) = plan else {
        panic!("{plan:?}");
    };
    let Some(Value::String(access)) = fields.get("access") else {
        panic!("{plan:?}");
    };
    let shards = fields.get("shards").map(|held| match held {
        Value::Array(ids) => ids
            .iter()
            .map(|id| match id {
                Value::Number(number) => number.as_exact_integer().unwrap(),
                other => panic!("{other:?}"),
            })
            .collect(),
        other => panic!("{other:?}"),
    });
    (access.clone(), shards)
}

/// ADR-0096 D3 — a read whose condition fixes the partition reads the span of
/// that partition's identities, in the one shard that holds them, and both its
/// plan and `EXPLAIN` say which shard.
#[test]
fn a_read_naming_a_partition_reads_its_span_in_one_shard() {
    let store = store();
    let mut session = customers(&store);
    session
        .run(
            "CREATE customers = { region: 'at', name: 'cy' }; \
             CREATE customers = { region: 'de', name: 'ada' }; \
             CREATE customers = { region: 'de', name: 'eve' }; \
             CREATE customers = { region: 'dk', name: 'bo' };",
        )
        .unwrap();
    let read = "SELECT * FROM customers WHERE region = 'de' AND name != 'eve';";
    let explained = match session.run(&format!("EXPLAIN {read}")).unwrap().last() {
        Some(Outcome::Value(plan)) => plan.clone(),
        other => panic!("{other:?}"),
    };
    // Shard 2 runs from 'de' to 'fr', and holds 'dk' too.
    assert_eq!(
        access_and_shards(&explained),
        ("span".to_owned(), Some(vec![2]))
    );
    let (records, plan) = match session.run(read).unwrap().last() {
        Some(Outcome::Records { records, plan, .. }) => (records.clone(), plan.clone()),
        other => panic!("{other:?}"),
    };
    assert_eq!(
        access_and_shards(&plan.to_value()),
        access_and_shards(&explained)
    );
    let names: Vec<String> = records
        .iter()
        .map(|(_, record)| format!("{record:?}"))
        .collect();
    assert_eq!(names.len(), 1, "{names:?}");
    assert!(names[0].contains("ada"), "{names:?}");
    // A condition that does not fix the partition reads the table.
    let explained = match session
        .run("EXPLAIN SELECT * FROM customers WHERE name = 'ada';")
        .unwrap()
        .last()
    {
        Some(Outcome::Value(plan)) => plan.clone(),
        other => panic!("{other:?}"),
    };
    assert_eq!(access_and_shards(&explained), ("scan".to_owned(), None));
}
