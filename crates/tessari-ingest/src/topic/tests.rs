//! The runner against a real store: declared on a running node, drained by
//! several members, dropped, halted, and stopped.
//!
//! Wall-clock waits, bounded, rather than a paused clock: each batch runs on the
//! blocking pool, and a paused runtime advances its clock whenever every task is
//! waiting — including while a batch is running — so the deadlines below would
//! pass before the work they wait for had a chance to finish.

// bgv-allow(unwrap): test code — a panic is the failure report, as in every suite here.
#![allow(clippy::unwrap_used, clippy::panic)]

use std::sync::Arc;
use std::time::{Duration, Instant};

use tessari_kv::{KvBackend, MemoryBackend};
use tessari_session::{Outcome, Session};
use tessari_storage::Store;
use tessari_types::Value;
use tokio::sync::watch;

use super::run_topic_consumers;

/// How long a condition may take before the test gives up on it.
const PATIENCE: Duration = Duration::from_secs(20);

async fn store_with(group: &str) -> Store {
    let store = Store::open(Arc::new(MemoryBackend::new()) as Arc<dyn KvBackend>)
        .await
        .unwrap();
    Session::new(&store)
        .run(&format!(
            "DEFINE NAMESPACE shop; USE NAMESPACE shop; DEFINE DATABASE live; \
             USE DATABASE live; DEFINE TOPIC orders; DEFINE TOPIC orders_dead; \
             DEFINE COLLECTION order_rows; {group}"
        ))
        .await
        .unwrap();
    store
}

async fn run(store: &Store, script: &str) -> Vec<Outcome> {
    Session::new(store)
        .run(&format!("USE NAMESPACE shop; USE DATABASE live; {script}"))
        .await
        .unwrap()
}

async fn rows(store: &Store) -> Vec<Value> {
    match run(store, "SELECT * FROM order_rows;").await.pop() {
        Some(Outcome::Records { records, .. }) => {
            records.into_iter().map(|(_, value)| value).collect()
        }
        other => panic!("not records: {other:?}"),
    }
}

/// Publish `count` messages, identities `1..=count`, in one transaction.
async fn publish(store: &Store, count: u64) {
    let mut script = String::from("BEGIN;");
    for at in 1..=count {
        script.push_str(&format!(
            " CREATE orders:{at} = {{ order_id: {at}, amount: {at} }};"
        ));
    }
    script.push_str(" COMMIT;");
    run(store, &script).await;
}

async fn until<F: std::future::Future<Output = bool>>(what: &str, mut holds: impl FnMut() -> F) {
    let started = Instant::now();
    while !holds().await {
        assert!(
            started.elapsed() < PATIENCE,
            "{what} did not happen in time"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// Drain `count` messages with a consumer of `members` members and answer the
/// store once every message has landed and the runner has stopped.
async fn drained(members: u32, count: u64) -> Store {
    let store = store_with(
        "DEFINE GROUP 'rows' ON TOPIC orders ACK DEADLINE 30s IN FLIGHT 64 \
         DELIVERIES 3 DEAD LETTER TO orders_dead;",
    )
    .await;
    let (stop, stopped) = watch::channel(false);
    let runner = tokio::spawn(run_topic_consumers(store.clone(), stopped));
    run(
        &store,
        &format!(
            "DEFINE TOPIC CONSUMER orders_in FROM orders GROUP 'rows' INTO order_rows \
             IDENTITY order_id MAP amount AS total ON FAILURE quarantine PARALLELISM {members};"
        ),
    )
    .await;
    publish(&store, count).await;
    let expected = usize::try_from(count).unwrap();
    until("every message landing", || async {
        rows(&store).await.len() == expected
    })
    .await;
    let applied = store.running().progress("orders_in").unwrap().applied;
    assert_eq!(applied, count, "a message was applied twice");
    stop.send_replace(true);
    tokio::time::timeout(PATIENCE, runner)
        .await
        .unwrap()
        .unwrap();
    assert!(
        store.running().progress("orders_in").is_none(),
        "a stopped runner still reports the consumer as running"
    );
    store
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_consumer_declared_on_a_running_node_applies_every_message_and_stops_when_told() {
    let store = drained(1, 50).await;
    let landed = rows(&store).await;
    for at in 1..=50_i64 {
        assert!(
            landed.iter().any(|record| {
                matches!(record, Value::Object(fields)
                    if fields.get("total") == Some(&Value::Number(tessari_types::Number::Integer(at))))
            }),
            "message {at} did not land"
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn four_members_share_the_work_with_nothing_lost_and_nothing_twice() {
    drained(4, 200).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_dropped_consumer_stops_and_a_halted_one_keeps_its_reason() {
    let store = store_with("DEFINE GROUP 'rows' ON TOPIC orders ACK DEADLINE 30s;").await;
    let (stop, stopped) = watch::channel(false);
    let runner = tokio::spawn(run_topic_consumers(store.clone(), stopped));
    run(
        &store,
        "DEFINE TOPIC CONSUMER orders_in FROM orders GROUP 'rows' INTO order_rows \
         IDENTITY order_id MAP amount AS total ON FAILURE stop;",
    )
    .await;
    publish(&store, 3).await;
    until("three records", || async { rows(&store).await.len() == 3 }).await;

    run(&store, "DROP TOPIC CONSUMER orders_in;").await;
    until("the dropped consumer leaving", || async {
        store.running().progress("orders_in").is_none()
    })
    .await;
    run(&store, "CREATE orders:'late' = { order_id: 4, amount: 4 };").await;
    tokio::time::sleep(Duration::from_millis(1_500)).await;
    assert_eq!(
        rows(&store).await.len(),
        3,
        "a dropped consumer went on writing"
    );

    run(
        &store,
        "DEFINE TOPIC CONSUMER orders_in FROM orders GROUP 'rows' INTO order_rows \
         IDENTITY order_id MAP amount AS total ON FAILURE stop;",
    )
    .await;
    until("the redeclared consumer resuming", || async {
        rows(&store).await.len() == 4
    })
    .await;
    run(&store, "CREATE orders:'bad' = { amount: 5 };").await;
    until("the consumer halting", || async {
        store
            .running()
            .progress("orders_in")
            .is_some_and(|progress| progress.halted)
    })
    .await;
    let reason = store
        .running()
        .progress("orders_in")
        .and_then(|progress| progress.last_error)
        .unwrap();
    assert!(reason.contains("could not be applied"), "{reason}");

    stop.send_replace(true);
    tokio::time::timeout(PATIENCE, runner)
        .await
        .unwrap()
        .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_consumer_declared_after_the_messages_takes_them_all_from_the_oldest() {
    let store = store_with(
        "DEFINE GROUP 'rows' ON TOPIC orders ACK DEADLINE 30s IN FLIGHT 64 \
         DELIVERIES 3 DEAD LETTER TO orders_dead;",
    )
    .await;
    publish(&store, 20).await;
    let (stop, stopped) = watch::channel(false);
    let runner = tokio::spawn(run_topic_consumers(store.clone(), stopped));
    run(
        &store,
        "DEFINE TOPIC CONSUMER orders_in FROM orders GROUP 'rows' INTO order_rows \
         IDENTITY order_id MAP amount AS total ON FAILURE quarantine;",
    )
    .await;
    until("the backlog landing", || async {
        rows(&store).await.len() == 20
    })
    .await;
    let applied = store.running().progress("orders_in").unwrap().applied;
    assert_eq!(applied, 20, "the backlog was not taken whole");
    stop.send_replace(true);
    tokio::time::timeout(PATIENCE, runner)
        .await
        .unwrap()
        .unwrap();
}

/// G043 C5: twenty runs of a thousand messages between four members. Ignored in
/// the suite for its length; run with `--ignored` under deliberate CPU load.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "G043 C5 evidence: 20 runs × 1 000 messages × 4 members; run with --ignored under CPU load"]
async fn twenty_runs_of_a_thousand_messages_between_four_members() {
    for _ in 0..20 {
        drained(4, 1_000).await;
    }
}

/// A topic consumer's writes are caller writes, so the destination's events
/// run in the batch's transaction — once per message applied (ADR-0110 D8).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_consumers_writes_run_the_destinations_events_once_each() {
    let store = store_with(
        "DEFINE GROUP 'rows' ON TOPIC orders ACK DEADLINE 30s IN FLIGHT 64 \
         DELIVERIES 3 DEAD LETTER TO orders_dead; DEFINE COLLECTION landed; \
         DEFINE EVENT tally ON order_rows FOR CREATE THEN CREATE landed = { total: $after.total };",
    )
    .await;
    let (stop, stopped) = watch::channel(false);
    let runner = tokio::spawn(run_topic_consumers(store.clone(), stopped));
    run(
        &store,
        "DEFINE TOPIC CONSUMER orders_in FROM orders GROUP 'rows' INTO order_rows \
         IDENTITY order_id MAP amount AS total ON FAILURE quarantine;",
    )
    .await;
    publish(&store, 40).await;
    until("every message landing", || async {
        rows(&store).await.len() == 40
    })
    .await;
    stop.send_replace(true);
    tokio::time::timeout(PATIENCE, runner)
        .await
        .unwrap()
        .unwrap();
    let Some(Outcome::Records { records, .. }) = run(&store, "SELECT * FROM landed;").await.pop()
    else {
        panic!("no answer");
    };
    let mut totals: Vec<i64> = records
        .iter()
        .map(|(_, value)| match value {
            Value::Object(fields) => match fields.get("total") {
                Some(Value::Number(tessari_types::Number::Integer(total))) => *total,
                other => panic!("{other:?}"),
            },
            other => panic!("{other:?}"),
        })
        .collect();
    totals.sort_unstable();
    assert_eq!(totals, (1..=40).collect::<Vec<_>>());
}

/// A consumer of a split topic reads every partition, each under the group it
/// holds there (ADR-0131).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_consumer_of_a_split_topic_applies_every_partitions_messages() {
    let store = store_with(
        "DEFINE TOPIC parts PARTITIONS 3 BY order_id; \
         DEFINE GROUP 'rows' ON TOPIC parts ACK DEADLINE 30s IN FLIGHT 64 \
         DELIVERIES 3 DEAD LETTER TO orders_dead;",
    )
    .await;
    let (stop, stopped) = watch::channel(false);
    let runner = tokio::spawn(run_topic_consumers(store.clone(), stopped));
    run(
        &store,
        "DEFINE TOPIC CONSUMER parts_in FROM parts GROUP 'rows' INTO order_rows \
         IDENTITY order_id MAP amount AS total ON FAILURE quarantine;",
    )
    .await;
    let mut script = String::from("BEGIN;");
    for at in 1..=60 {
        script.push_str(&format!(
            " CREATE parts = {{ order_id: {at}, amount: {at} }};"
        ));
    }
    script.push_str(" COMMIT;");
    run(&store, &script).await;
    // The keys spread over every partition, so a member missing one leaves
    // its messages unapplied.
    let Some(Outcome::Value(Value::Object(report))) =
        run(&store, "INFO FOR TOPIC parts;").await.pop()
    else {
        panic!("INFO FOR TOPIC parts answered no report");
    };
    let Some(Value::Array(each)) = report.get("each") else {
        panic!("{report:?}");
    };
    assert!(
        each.iter().all(|partition| !matches!(partition,
            Value::Object(fields) if fields.get("last") == Some(&Value::Number(tessari_types::Number::Integer(0))))),
        "a partition received nothing: {each:?}"
    );
    until("every message landing", || async {
        rows(&store).await.len() == 60
    })
    .await;
    assert_eq!(store.running().progress("parts_in").unwrap().applied, 60);
    stop.send_replace(true);
    tokio::time::timeout(PATIENCE, runner)
        .await
        .unwrap()
        .unwrap();
}
