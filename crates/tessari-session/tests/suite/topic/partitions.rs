//! A topic split into partitions, each a topic of its own (G079 W6, ADR-0131).

use tessari_session::{Error, RunNow};
use tessari_types::Value;

use super::super::key_value::{on_each_backend, refused, rows, run, value};
use super::opened;

fn field(session: &mut tessari_session::Session<'_>, script: &str, name: &str) -> Value {
    let Value::Object(fields) = value(session, script) else {
        panic!("{script} answered no object");
    };
    fields
        .get(name)
        .cloned()
        .unwrap_or_else(|| panic!("{script}: no `{name}` in {fields:?}"))
}

#[test]
fn a_partitioned_topic_is_written_back_and_described_with_its_partitions() {
    on_each_backend(|backend| {
        let mut session = opened(&backend.store);
        run(
            &mut session,
            "DEFINE TOPIC orders PARTITIONS 3 BY customer RETAIN 7d;",
        );
        let written = field(&mut session, "INFO FOR TABLE orders;", "definition");
        assert_eq!(
            written,
            Value::from("DEFINE TOPIC orders RETAIN 168h PARTITIONS 3 BY customer;\n"),
            "{}",
            backend.name
        );
        assert_eq!(
            field(&mut session, "INFO FOR TOPIC orders;", "partitions"),
            Value::from(3),
            "{}",
            backend.name
        );
        assert_eq!(
            field(&mut session, "INFO FOR TOPIC orders;", "by"),
            Value::from("customer")
        );
        // A topic that was not split says so, rather than one partition.
        assert_eq!(
            field(&mut session, "INFO FOR TOPIC events;", "partitions"),
            Value::Null
        );
        // The partitions are not tables anybody can name or is shown.
        let Value::Object(database) = value(&mut session, "INFO FOR DATABASE;") else {
            panic!("INFO FOR DATABASE answered no object");
        };
        let listed = format!("{database:?}");
        assert!(listed.contains("orders"), "{listed}");
        assert!(!listed.contains("orders#"), "{listed}");
    });
}

#[test]
fn a_partition_count_outside_its_bounds_is_refused() {
    on_each_backend(|backend| {
        let mut session = opened(&backend.store);
        for script in [
            "DEFINE TOPIC t PARTITIONS 0;",
            "DEFINE TOPIC t PARTITIONS 1025;",
        ] {
            let said = refused(&mut session, script);
            assert!(
                said.contains("1 to 1024"),
                "{}: {script}: {said}",
                backend.name
            );
        }
    });
}

/// What one append answers to `RETURN POSITION`: its partition and position.
fn placed(session: &mut tessari_session::Session<'_>, script: &str) -> (Value, Value) {
    let Value::Array(answered) = value(session, script) else {
        panic!("{script} answered no list");
    };
    let [Value::Object(place)] = answered.as_slice() else {
        panic!("{script} answered {answered:?}");
    };
    (
        place.get("partition").cloned().unwrap_or(Value::None),
        place.get("position").cloned().unwrap_or(Value::None),
    )
}

/// The last position of each partition, in order.
fn lasts(session: &mut tessari_session::Session<'_>, topic: &str) -> Vec<Value> {
    let Value::Array(each) = field(session, &format!("INFO FOR TOPIC {topic};"), "each") else {
        panic!("INFO FOR TOPIC {topic} lists no partitions");
    };
    each.iter()
        .map(|partition| match partition {
            Value::Object(fields) => fields.get("last").cloned().unwrap_or(Value::None),
            other => other.clone(),
        })
        .collect()
}

#[test]
fn an_append_with_a_key_lands_in_the_partition_a_kafka_client_would_choose() {
    on_each_backend(|backend| {
        let mut session = opened(&backend.store);
        run(
            &mut session,
            "DEFINE TOPIC orders PARTITIONS 3 BY customer;",
        );
        // murmur2 of the key's bytes, as the Kafka clients' default partitioner
        // computes it: "user-42" → 1, "ключ" (UTF-8) → 2, the integer 21 as
        // the text "21" → 0.
        for (script, partition, position) in [
            (
                "CREATE orders = { customer: 'user-42' } RETURN POSITION;",
                1,
                1,
            ),
            (
                "CREATE orders = { customer: 'ключ' } RETURN POSITION;",
                2,
                1,
            ),
            (
                "CREATE orders = { customer: 'user-42', n: 2 } RETURN POSITION;",
                1,
                2,
            ),
            ("CREATE orders = { customer: 21 } RETURN POSITION;", 0, 1),
        ] {
            assert_eq!(
                placed(&mut session, script),
                (Value::from(partition), Value::from(position)),
                "{}: {script}",
                backend.name
            );
        }
        assert_eq!(
            lasts(&mut session, "orders"),
            vec![Value::from(1), Value::from(2), Value::from(1)],
            "{}",
            backend.name
        );
        assert_eq!(rows(&mut session, "READ FROM orders PARTITION 1;"), 2);
        assert_eq!(rows(&mut session, "READ FROM orders PARTITION 0;"), 1);
    });
}

#[test]
fn an_append_with_no_key_takes_the_partitions_in_turn() {
    on_each_backend(|backend| {
        let mut session = opened(&backend.store);
        run(
            &mut session,
            "DEFINE TOPIC jobs PARTITIONS 3;\n\
             DEFINE TOPIC orders PARTITIONS 2 BY customer;",
        );
        for n in 1..=3 {
            run(&mut session, &format!("CREATE jobs = {{ n: {n} }};"));
        }
        assert_eq!(
            lasts(&mut session, "jobs"),
            vec![Value::from(1), Value::from(1), Value::from(1)],
            "{}",
            backend.name
        );
        // A message without the key, as a Kafka record with a null key.
        run(&mut session, "INSERT INTO orders (n) VALUES (1), (2);");
        assert_eq!(
            lasts(&mut session, "orders"),
            vec![Value::from(1), Value::from(1)],
            "{}",
            backend.name
        );
    });
}

#[test]
fn a_read_of_a_split_topic_names_its_partition() {
    on_each_backend(|backend| {
        let mut session = opened(&backend.store);
        run(
            &mut session,
            "DEFINE TOPIC orders PARTITIONS 2 BY customer;",
        );
        assert!(
            matches!(
                session.run_now("READ FROM orders;"),
                Err(Error::PartitionNotNamed { .. })
            ),
            "{}",
            backend.name
        );
        assert!(matches!(
            session.run_now("READ FROM orders PARTITION 2;"),
            Err(Error::NoSuchPartition { .. })
        ));
        // A topic that was not split is its own one partition.
        run(&mut session, "CREATE events = { n: 1 };");
        assert_eq!(rows(&mut session, "READ FROM events PARTITION 0;"), 1);
        assert!(matches!(
            session.run_now("READ FROM events PARTITION 1;"),
            Err(Error::NoSuchPartition { .. })
        ));
        // The parent holds nothing: a write that is not an append is refused
        // rather than left where no partition's reader would see it.
        let said = refused(&mut session, "UPSERT orders:'a' = { customer: 'x' };");
        assert!(said.contains("split"), "{}: {said}", backend.name);
    });
}

#[test]
fn dropping_a_split_topic_drops_its_partitions() {
    on_each_backend(|backend| {
        let mut session = opened(&backend.store);
        run(
            &mut session,
            "DEFINE TOPIC orders PARTITIONS 2;\n\
             CREATE orders = { n: 1 };\n\
             CREATE orders = { n: 2 };\n\
             DROP TOPIC orders;",
        );
        // The same name declared again is a new topic: a partition left behind
        // would hold its name, or its messages.
        run(&mut session, "DEFINE TOPIC orders PARTITIONS 2;");
        assert_eq!(
            lasts(&mut session, "orders"),
            vec![Value::from(0), Value::from(0)],
            "{}",
            backend.name
        );
    });
}

/// A store an older build wrote keeps its format until it is finalized: a split
/// topic, which an older build would append to as one topic nobody reads, is
/// refused rather than moving the store to 9 (ADR-0118, ADR-0131).
#[test]
fn a_store_holding_format_eight_refuses_a_split_topic() {
    use std::sync::Arc;

    use tessari_encoding::{FormatVersion, FormatVersionKey, StoreKey, StoreValue};
    use tessari_kv::{KvBackend, MemoryBackend, WriteBatch};
    use tessari_session::{Now, Session};
    use tessari_storage::Store;

    let backend = Arc::new(MemoryBackend::new()) as Arc<dyn KvBackend>;
    drop(Store::open(Arc::clone(&backend)).now().unwrap());
    backend
        .apply(WriteBatch::new().put(
            FormatVersionKey::keyspace(),
            FormatVersionKey.encode(),
            FormatVersion::new(8).encode(),
        ))
        .now()
        .unwrap();
    let store = Store::open(Arc::clone(&backend)).now().unwrap();
    let mut session = Session::new(&store);
    session
        .run_now(
            "DEFINE NAMESPACE n; USE NAMESPACE n; DEFINE DATABASE d; USE DATABASE d;\n\
             DEFINE TOPIC events;",
        )
        .unwrap();
    let refused = session
        .run_now("DEFINE TOPIC orders PARTITIONS 2;")
        .unwrap_err();
    assert!(
        matches!(
            refused,
            Error::FormatNotFinalized {
                needs: 9,
                holds: 8,
                ..
            }
        ),
        "{refused:?}"
    );
    // Nothing of the refused declaration was kept.
    let listed = format!("{:?}", session.run_now("INFO FOR DATABASE;").unwrap());
    assert!(listed.contains("events"), "{listed}");
    assert!(!listed.contains("orders"), "{listed}");
}

#[test]
fn a_dead_letter_to_a_split_topic_lands_in_one_of_its_partitions() {
    on_each_backend(|backend| {
        let mut session = opened(&backend.store);
        run(
            &mut session,
            "DEFINE TOPIC dead PARTITIONS 2;\n\
             DEFINE GROUP 'billing' ON TOPIC events ACK DEADLINE 30s DELIVERIES 1 \
             DEAD LETTER TO dead;\n\
             CREATE events = { n: 1 };",
        );
        let read = "READ FROM events FOR CONSUMER 'billing';";
        assert_eq!(rows(&mut session, read), 1);
        run(&mut session, "NACK events FOR CONSUMER 'billing' AT 1;");
        // Delivered as often as the group allows, so the next read letters it.
        assert_eq!(rows(&mut session, read), 0, "{}", backend.name);
        let landed: i64 = lasts(&mut session, "dead")
            .iter()
            .map(|last| match last {
                Value::Number(tessari_types::Number::Integer(count)) => *count,
                _ => 0,
            })
            .sum();
        assert_eq!(landed, 1, "{}", backend.name);
    });
}

/// A partition's entry in `INFO FOR TOPIC`.
fn partition_report(
    session: &mut tessari_session::Session<'_>,
    topic: &str,
    partition: usize,
) -> std::collections::BTreeMap<String, Value> {
    let Value::Array(each) = field(session, &format!("INFO FOR TOPIC {topic};"), "each") else {
        panic!("INFO FOR TOPIC {topic} lists no partitions");
    };
    match each.get(partition) {
        Some(Value::Object(report)) => report.clone(),
        other => panic!("partition {partition} of {topic}: {other:?}"),
    }
}

#[test]
fn a_group_on_a_split_topic_holds_each_partition_on_its_own() {
    on_each_backend(|backend| {
        let mut session = opened(&backend.store);
        // "user-42" lands in partition 0 of two, "abc" in partition 1.
        run(
            &mut session,
            "DEFINE TOPIC orders PARTITIONS 2 BY customer;\n\
             DEFINE GROUP 'billing' ON TOPIC orders ACK DEADLINE 30s;\n\
             CREATE orders = { customer: 'user-42' };\n\
             CREATE orders = { customer: 'user-42', n: 2 };\n\
             CREATE orders = { customer: 'abc' };",
        );
        let first = "READ FROM orders PARTITION 0 FOR CONSUMER 'billing';";
        assert_eq!(rows(&mut session, first), 1, "{}", backend.name);
        // One in flight in partition 0 holds that partition and no other.
        assert_eq!(rows(&mut session, first), 0, "{}", backend.name);
        assert_eq!(
            rows(
                &mut session,
                "READ FROM orders PARTITION 1 FOR CONSUMER 'billing';"
            ),
            1,
            "{}",
            backend.name
        );
        assert!(matches!(
            session.run_now("ACK orders FOR CONSUMER 'billing' AT 1;"),
            Err(Error::PartitionNotNamed { .. })
        ));
        assert_eq!(
            value(
                &mut session,
                "ACK orders PARTITION 0 FOR CONSUMER 'billing' AT 1;"
            ),
            Value::from(1),
            "{}",
            backend.name
        );
        assert_eq!(rows(&mut session, first), 1, "{}", backend.name);
        let report = partition_report(&mut session, "orders", 0);
        let Some(Value::Object(groups)) = report.get("groups") else {
            panic!("{report:?}");
        };
        let Some(Value::Object(billing)) = groups.get("billing") else {
            panic!("{groups:?}");
        };
        assert_eq!(
            billing.get("committed"),
            Some(&Value::from(1)),
            "{billing:?}"
        );
        assert_eq!(
            billing.get("position"),
            Some(&Value::from(2)),
            "{billing:?}"
        );
        // Dropped from every partition at once.
        run(&mut session, "DROP GROUP 'billing' ON TOPIC orders;");
        let report = partition_report(&mut session, "orders", 1);
        assert_eq!(
            report.get("groups"),
            Some(&Value::Object(std::collections::BTreeMap::new())),
            "{}: {report:?}",
            backend.name
        );
    });
}
