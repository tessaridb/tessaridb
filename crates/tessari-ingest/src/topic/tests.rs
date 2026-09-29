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

fn store_with(group: &str) -> Store {
    let store = Store::open(Arc::new(MemoryBackend::new()) as Arc<dyn KvBackend>).unwrap();
    Session::new(&store)
        .run(&format!(
            "DEFINE NAMESPACE shop; USE NAMESPACE shop; DEFINE DATABASE live; \
             USE DATABASE live; DEFINE TOPIC orders; DEFINE TOPIC orders_dead; \
             DEFINE COLLECTION order_rows; {group}"
        ))
        .unwrap();
    store
}

fn run(store: &Store, script: &str) -> Vec<Outcome> {
    Session::new(store)
        .run(&format!("USE NAMESPACE shop; USE DATABASE live; {script}"))
        .unwrap()
}

fn rows(store: &Store) -> Vec<Value> {
    match run(store, "SELECT * FROM order_rows;").pop() {
        Some(Outcome::Records { records, .. }) => {
            records.into_iter().map(|(_, value)| value).collect()
        }
        other => panic!("not records: {other:?}"),
    }
}

/// Publish `count` messages, identities `1..=count`, in one transaction.
fn publish(store: &Store, count: u64) {
    let mut script = String::from("BEGIN;");
    for at in 1..=count {
        script.push_str(&format!(
            " CREATE orders:{at} = {{ order_id: {at}, amount: {at} }};"
        ));
    }
    script.push_str(" COMMIT;");
    run(store, &script);
}

async fn until(what: &str, mut holds: impl FnMut() -> bool) {
    let started = Instant::now();
    while !holds() {
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
    );
    let (stop, stopped) = watch::channel(false);
    let runner = tokio::spawn(run_topic_consumers(store.clone(), stopped));
    run(
        &store,
        &format!(
            "DEFINE TOPIC CONSUMER orders_in FROM orders GROUP 'rows' INTO order_rows \
             IDENTITY order_id MAP amount AS total ON FAILURE quarantine PARALLELISM {members};"
        ),
    );
    publish(&store, count);
    let expected = usize::try_from(count).unwrap();
    until("every message landing", || rows(&store).len() == expected).await;
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
    let landed = rows(&store);
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
    let store = store_with("DEFINE GROUP 'rows' ON TOPIC orders ACK DEADLINE 30s;");
    let (stop, stopped) = watch::channel(false);
    let runner = tokio::spawn(run_topic_consumers(store.clone(), stopped));
    run(
        &store,
        "DEFINE TOPIC CONSUMER orders_in FROM orders GROUP 'rows' INTO order_rows \
         IDENTITY order_id MAP amount AS total ON FAILURE stop;",
    );
    publish(&store, 3);
    until("three records", || rows(&store).len() == 3).await;

    run(&store, "DROP TOPIC CONSUMER orders_in;");
    until("the dropped consumer leaving", || {
        store.running().progress("orders_in").is_none()
    })
    .await;
    run(&store, "CREATE orders:'late' = { order_id: 4, amount: 4 };");
    tokio::time::sleep(Duration::from_millis(1_500)).await;
    assert_eq!(rows(&store).len(), 3, "a dropped consumer went on writing");

    run(
        &store,
        "DEFINE TOPIC CONSUMER orders_in FROM orders GROUP 'rows' INTO order_rows \
         IDENTITY order_id MAP amount AS total ON FAILURE stop;",
    );
    until("the redeclared consumer resuming", || {
        rows(&store).len() == 4
    })
    .await;
    run(&store, "CREATE orders:'bad' = { amount: 5 };");
    until("the consumer halting", || {
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

/// G043 C5: twenty runs of a thousand messages between four members. Ignored in
/// the suite for its length; run with `--ignored` under deliberate CPU load.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "G043 C5 evidence: 20 runs × 1 000 messages × 4 members; run with --ignored under CPU load"]
async fn twenty_runs_of_a_thousand_messages_between_four_members() {
    for _ in 0..20 {
        drained(4, 1_000).await;
    }
}
