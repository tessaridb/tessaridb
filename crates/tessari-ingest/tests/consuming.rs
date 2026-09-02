//! What a running consumer does, against a source that can be made to fail.
//!
//! Every property here is one a real broker cannot demonstrate inside
//! `cargo test`: a message that will never parse, a store commit that succeeds
//! while the offset commit does not, two threads that must genuinely overlap, a
//! shutdown that must not abandon a batch. That is the whole reason the runner
//! consumes a trait (ADR-0024 §3).
//!
//! # The one it exists for
//!
//! `a_crash_between_the_two_commits_duplicates_and_never_loses` is the delivery
//! guarantee itself. The store commit is made to succeed and the offset commit
//! to fail, which is exactly the window ADR-0023 §5 tabulates, and the assertion
//! is that redelivery converges rather than doubling.

#![allow(clippy::panic, clippy::unwrap_used)]

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tessari_ingest::{Broker, Message, Runner, Source, SourceError};
use tessari_kv::{KvBackend, MemoryBackend};
use tessari_session::{Outcome, Session};
use tessari_storage::{ConsumerDefinition, Store};
use tessari_types::Value;

const DECLARE: &str = "DEFINE CONSUMER orders_in \
     FROM 'broker:9092' TOPIC 'orders' GROUP 'shop-orders' \
     FORMAT json INTO orders IDENTITY order_id \
     MAP amount AS total ON FAILURE quarantine;";

/// A store holding `prod.shop.orders`, with a consumer declared.
fn declared(policy: &str, parallelism: &str) -> Store {
    declared_on(
        &(Arc::new(MemoryBackend::new()) as Arc<dyn KvBackend>),
        policy,
        parallelism,
    )
}

/// The same, on a backend the caller keeps — so it can be reopened.
fn declared_on(backend: &Arc<dyn KvBackend>, policy: &str, parallelism: &str) -> Store {
    let store = Store::open(Arc::clone(backend)).unwrap();
    let mut session = Session::new(&store);
    session
        .run(
            "DEFINE NAMESPACE prod; USE NAMESPACE prod; \
             DEFINE DATABASE shop; USE DATABASE shop; DEFINE COLLECTION orders;",
        )
        .unwrap();
    session
        .run(
            &DECLARE
                .replace("ON FAILURE quarantine", &format!("ON FAILURE {policy}"))
                .replace(";", &format!("{parallelism};")),
        )
        .unwrap();
    store
}

/// The records in `prod.shop.orders`, by identity.
fn landed(store: &Store) -> BTreeMap<String, Value> {
    let mut session = Session::new(store);
    let answered = session
        .run("USE NAMESPACE prod; USE DATABASE shop; SELECT * FROM orders;")
        .unwrap();
    let Some(Outcome::Records { records, .. }) = answered.last() else {
        panic!("not records: {answered:?}");
    };
    records
        .iter()
        .map(|(id, value)| (id.to_string(), value.clone()))
        .collect()
}

/// One JSON message.
fn message(partition: i32, offset: i64, body: &str) -> Message {
    Message {
        partition,
        offset,
        payload: body.as_bytes().to_vec(),
    }
}

/// A source the test hands messages to, and can make fail on command.
#[derive(Default)]
struct Scripted {
    /// What is left to hand out.
    waiting: Mutex<Vec<Message>>,
    /// How many times `commit` has been called.
    commits: AtomicUsize,
    /// Whether `commit` should refuse.
    commit_fails: bool,
    /// How many times `poll` has been called, so a test can see overlap.
    polls: Arc<AtomicUsize>,
    /// How long each `poll` pretends to wait, so two threads must overlap to
    /// finish in time.
    slow: Duration,
    /// Set once a message has been handed out.
    handed: Arc<AtomicBool>,
}

impl Source for Scripted {
    fn poll(&mut self, _patience: Duration) -> Result<Option<Message>, SourceError> {
        self.polls.fetch_add(1, Ordering::Relaxed);
        if !self.slow.is_zero() {
            std::thread::sleep(self.slow);
        }
        let held = self
            .waiting
            .lock()
            .map_err(|_| SourceError("poisoned".to_owned()))?
            .pop();
        if held.is_some() {
            self.handed.store(true, Ordering::Relaxed);
        }
        Ok(held)
    }

    fn commit(&mut self) -> Result<(), SourceError> {
        self.commits.fetch_add(1, Ordering::Relaxed);
        if self.commit_fails {
            return Err(SourceError("the broker refused the offset".to_owned()));
        }
        Ok(())
    }
}

/// Hands every consumer the same script of messages.
struct Handing {
    messages: Vec<Message>,
    commit_fails: bool,
    polls: Arc<AtomicUsize>,
    slow: Duration,
    /// Set once a message has been handed out, so a test can stop the runner at
    /// a known point rather than after a sleep it hopes is long enough.
    handed: Arc<AtomicBool>,
}

impl Broker for Handing {
    fn open(&self, _definition: &ConsumerDefinition) -> Result<Box<dyn Source>, SourceError> {
        let mut waiting = self.messages.clone();
        // Popped from the back, so reversing here hands them out in order.
        waiting.reverse();
        Ok(Box::new(Scripted {
            waiting: Mutex::new(waiting),
            commits: AtomicUsize::new(0),
            commit_fails: self.commit_fails,
            polls: Arc::clone(&self.polls),
            slow: self.slow,
            handed: Arc::clone(&self.handed),
        }))
    }
}

/// Run the consumers until `settled` holds, or give up after a second.
///
/// A condition rather than a fixed wait: a fixed one is either flaky on a busy
/// machine or slow on an idle one, and this file already has one test whose
/// whole point is timing.
fn until(store: &Store, broker: Arc<dyn Broker>, settled: impl Fn(&Store) -> bool) {
    let mut started = Runner::start(store, &broker).unwrap();
    let deadline = Instant::now()
        .checked_add(Duration::from_secs(5))
        .unwrap_or_else(Instant::now);
    while Instant::now() < deadline && !settled(store) {
        std::thread::sleep(Duration::from_millis(10));
    }
    started.stop();
}

#[test]
fn a_message_becomes_the_record_the_declaration_says() {
    let store = declared("quarantine", "");
    let broker: Arc<dyn Broker> = Arc::new(Handing {
        messages: vec![message(0, 1, r#"{"order_id": 7, "amount": 500}"#)],
        commit_fails: false,
        polls: Arc::new(AtomicUsize::new(0)),
        slow: Duration::ZERO,
        handed: Arc::new(AtomicBool::new(false)),
    });
    until(&store, broker, |store| !landed(store).is_empty());

    let records = landed(&store);
    assert_eq!(records.len(), 1, "{records:?}");
    let Some(Value::Object(fields)) = records.values().next() else {
        panic!("not an object: {records:?}");
    };
    assert_eq!(fields.get("total"), Some(&Value::from(500_i64)));
    assert_eq!(
        fields.get("amount"),
        None,
        "the message's own field name landed, so the mapping was not applied"
    );
}

#[test]
fn a_crash_between_the_two_commits_duplicates_and_never_loses() {
    // **The delivery guarantee.** The store commit succeeds and the offset
    // commit fails, which is the one-step window ADR-0023 §5 tabulates. The
    // broker then redelivers, and the assertion is that the record converges
    // rather than doubling — because the identity comes from the message.
    let store = declared("quarantine", "");
    let same = r#"{"order_id": 7, "amount": 500}"#;
    let broker: Arc<dyn Broker> = Arc::new(Handing {
        // The same message three times, which is what a redelivery is.
        messages: vec![
            message(0, 1, same),
            message(0, 1, same),
            message(0, 1, same),
        ],
        commit_fails: true,
        polls: Arc::new(AtomicUsize::new(0)),
        slow: Duration::ZERO,
        handed: Arc::new(AtomicBool::new(false)),
    });
    until(&store, broker, |store| !landed(store).is_empty());

    let records = landed(&store);
    // **Non-empty is half the proof and is the half about ordering.** The offset
    // commit fails every time, so if it ran *before* the store commit the batch
    // would abort and nothing would land. Verified by inverting the two lines in
    // `Consuming::once`: this assertion then fails with 0 records.
    //
    // **Exactly one is the other half.** Three deliveries of one message converge
    // because the identity comes from the payload, which is what makes
    // at-least-once usable rather than merely honest.
    assert_eq!(
        records.len(),
        1,
        "three deliveries of one message became {} records: {records:?}",
        records.len()
    );
    // And the data is right, not merely single: a converging write that lost the
    // fields would also pass a count.
    let Some(Value::Object(fields)) = records.values().next() else {
        panic!("not an object");
    };
    assert_eq!(fields.get("total"), Some(&Value::from(500_i64)));
}

#[test]
fn a_poison_message_does_not_stall_the_partition() {
    // The failure the reference system documents about itself: one malformed
    // message blocks consumers, and a stalled consumer triggers a group
    // rebalance, which degrades every other consumer in the group. `quarantine`
    // exists so that one bad payload costs one record.
    let store = declared("quarantine", "");
    let broker: Arc<dyn Broker> = Arc::new(Handing {
        messages: vec![
            message(0, 1, r#"{"order_id": 1, "amount": 10}"#),
            message(0, 2, "{ this is not json"),
            message(0, 3, r#"{"order_id": 3, "amount": 30}"#),
        ],
        commit_fails: false,
        polls: Arc::new(AtomicUsize::new(0)),
        slow: Duration::ZERO,
        handed: Arc::new(AtomicBool::new(false)),
    });
    until(&store, broker, |store| landed(store).len() >= 2);

    let records = landed(&store);
    assert_eq!(
        records.len(),
        2,
        "the messages after the poison one did not land: {records:?}"
    );
}

#[test]
fn stop_halts_this_consumer_and_says_why() {
    // The other policy, and the reason there are two: an operator who chose
    // `stop` wants to look at the message before anything else moves.
    let store = declared("stop", "");
    let broker: Arc<dyn Broker> = Arc::new(Handing {
        messages: vec![
            message(0, 1, "{ this is not json"),
            message(0, 2, r#"{"order_id": 2, "amount": 20}"#),
        ],
        commit_fails: false,
        polls: Arc::new(AtomicUsize::new(0)),
        slow: Duration::ZERO,
        handed: Arc::new(AtomicBool::new(false)),
    });
    let mut started = Runner::start(&store, &broker).unwrap();
    let deadline = Instant::now()
        .checked_add(Duration::from_secs(5))
        .unwrap_or_else(Instant::now);
    while Instant::now() < deadline && store.running().progress("orders_in").is_some() {
        std::thread::sleep(Duration::from_millis(10));
    }
    started.stop();

    assert!(
        landed(&store).is_empty(),
        "a stopping consumer applied the batch it refused"
    );
    assert!(
        store.running().progress("orders_in").is_none(),
        "a halted consumer still reports itself as running"
    );
}

#[test]
fn parallelism_runs_more_than_one_consumer_at_a_time() {
    // ADR-0023 names this explicitly: a parallelism setting that does nothing is
    // the defect the reference system shipped, and *a knob nobody measures is
    // believed by everybody*. So it is measured.
    //
    // Each poll sleeps, so N threads finish N polls in about the time one thread
    // takes for one. The assertion is on **wall clock against poll count**,
    // which serial execution cannot satisfy however fast the machine is.
    let store = declared("quarantine", " PARALLELISM 4");
    let polls = Arc::new(AtomicUsize::new(0));
    let broker: Arc<dyn Broker> = Arc::new(Handing {
        messages: Vec::new(),
        commit_fails: false,
        polls: Arc::clone(&polls),
        slow: Duration::from_millis(150),
        handed: Arc::new(AtomicBool::new(false)),
    });
    let began = Instant::now();
    let mut started = Runner::start(&store, &broker).unwrap();
    assert_eq!(started.threads(), 4, "four consumers were not started");

    // Wait until at least four polls have happened.
    let deadline = Instant::now()
        .checked_add(Duration::from_secs(5))
        .unwrap_or_else(Instant::now);
    while Instant::now() < deadline && polls.load(Ordering::Relaxed) < 4 {
        std::thread::sleep(Duration::from_millis(5));
    }
    let taken = began.elapsed();
    started.stop();

    assert!(
        polls.load(Ordering::Relaxed) >= 4,
        "fewer than four polls happened at all"
    );
    assert!(
        taken < Duration::from_millis(450),
        "four 150ms polls took {taken:?}, which is what running them one after \
         another looks like — the parallelism setting did nothing"
    );
}

#[test]
fn a_consumer_that_was_started_reports_what_it_has_applied() {
    // The complaint the reference system's own improvement proposal is about: a
    // consumer that can be declared and not observed.
    let store = declared("quarantine", "");
    let broker: Arc<dyn Broker> = Arc::new(Handing {
        messages: vec![
            message(0, 1, r#"{"order_id": 1, "amount": 10}"#),
            message(0, 2, "{ not json"),
        ],
        commit_fails: false,
        polls: Arc::new(AtomicUsize::new(0)),
        slow: Duration::ZERO,
        handed: Arc::new(AtomicBool::new(false)),
    });
    let mut started = Runner::start(&store, &broker).unwrap();
    let deadline = Instant::now()
        .checked_add(Duration::from_secs(5))
        .unwrap_or_else(Instant::now);
    while Instant::now() < deadline
        && store
            .running()
            .progress("orders_in")
            .is_none_or(|progress| progress.applied == 0)
    {
        std::thread::sleep(Duration::from_millis(10));
    }
    let progress = store.running().progress("orders_in").unwrap();
    started.stop();

    assert_eq!(progress.applied, 1);
    assert_eq!(progress.quarantined, 1);
    assert_eq!(
        progress.positions.get(&0),
        Some(&2),
        "the position it reached was not recorded"
    );
}

#[test]
fn stopping_joins_the_threads_rather_than_abandoning_them() {
    // Shutdown that abandons an in-flight batch turns at-least-once into a lie
    // about which end: the offset may have moved while the store commit had not.
    let store = declared("quarantine", " PARALLELISM 2");
    let broker: Arc<dyn Broker> = Arc::new(Handing {
        messages: Vec::new(),
        commit_fails: false,
        polls: Arc::new(AtomicUsize::new(0)),
        slow: Duration::from_millis(50),
        handed: Arc::new(AtomicBool::new(false)),
    });
    let mut started = Runner::start(&store, &broker).unwrap();
    assert_eq!(started.threads(), 2);
    started.stop();
    assert_eq!(started.threads(), 0, "stop() left threads behind");
    assert!(
        store.running().progress("orders_in").is_none(),
        "a stopped consumer still reports itself as running"
    );
    // Idempotent, so the explicit call and the drop do not fight.
    started.stop();
}

#[test]
fn a_declaration_whose_destination_has_gone_is_skipped_rather_than_fatal() {
    // One broken consumer must not stop a node from serving. It reports itself
    // as not running here, which is the same thing `INFO FOR CONSUMER` says.
    let store = declared("quarantine", "");
    Session::new(&store)
        .run("USE NAMESPACE prod; USE DATABASE shop; DROP TABLE orders;")
        .unwrap();
    let broker: Arc<dyn Broker> = Arc::new(Handing {
        messages: vec![message(0, 1, r#"{"order_id": 1, "amount": 10}"#)],
        commit_fails: false,
        polls: Arc::new(AtomicUsize::new(0)),
        slow: Duration::ZERO,
        handed: Arc::new(AtomicBool::new(false)),
    });
    let mut started = Runner::start(&store, &broker).unwrap();
    assert_eq!(
        started.threads(),
        0,
        "a consumer with no destination started"
    );
    started.stop();
}

#[test]
fn a_store_with_nothing_declared_starts_nothing() {
    let backend = Arc::new(MemoryBackend::new()) as Arc<dyn KvBackend>;
    let store = Store::open(backend).unwrap();
    let broker: Arc<dyn Broker> = Arc::new(Handing {
        messages: Vec::new(),
        commit_fails: false,
        polls: Arc::new(AtomicUsize::new(0)),
        slow: Duration::ZERO,
        handed: Arc::new(AtomicBool::new(false)),
    });
    let mut started = Runner::start(&store, &broker).unwrap();
    assert_eq!(started.threads(), 0);
    started.stop();
}

#[test]
fn stopping_finishes_the_batch_in_flight_rather_than_abandoning_it() {
    // **The graceful-shutdown property.** A consumer told to stop mid-batch must
    // write what it has already taken and commit its offset, then return. The
    // alternative is not merely untidy: the messages were taken from the broker
    // and the offset may or may not reflect that, so abandoning them turns
    // at-least-once into a claim nobody can act on.
    //
    // Timed off the source rather than off a sleep: the stop is issued the
    // instant a message has actually been handed out, which is the moment the
    // runner is inside a batch.
    let store = declared("quarantine", "");
    let handed = Arc::new(AtomicBool::new(false));
    let broker: Arc<dyn Broker> = Arc::new(Handing {
        messages: vec![message(0, 1, r#"{"order_id": 7, "amount": 500}"#)],
        commit_fails: false,
        polls: Arc::new(AtomicUsize::new(0)),
        // Every poll after the first one blocks, so the runner is still inside
        // its gather loop when the stop arrives.
        slow: Duration::from_millis(120),
        handed: Arc::clone(&handed),
    });

    let mut started = Runner::start(&store, &broker).unwrap();
    let deadline = Instant::now()
        .checked_add(Duration::from_secs(5))
        .unwrap_or_else(Instant::now);
    while Instant::now() < deadline && !handed.load(Ordering::Relaxed) {
        std::thread::sleep(Duration::from_millis(2));
    }
    assert!(
        handed.load(Ordering::Relaxed),
        "no message was ever handed out"
    );
    started.stop();

    // `stop` has returned, so every thread has been joined. Nothing else is
    // running that could still write, which is what makes this assertion about
    // the shutdown rather than about a race the test happened to win.
    let records = landed(&store);
    assert_eq!(
        records.len(),
        1,
        "a consumer stopped mid-batch abandoned the message it had already taken: {records:?}"
    );
    assert_eq!(
        store.running().progress("orders_in").as_ref().map(|_| ()),
        None,
        "a stopped consumer still reports itself as running"
    );
}

#[test]
fn a_restarted_node_starts_what_the_catalog_declares() {
    // **The evidence S5 actually asks for is a restart, not a start.** A runner
    // that starts consumers on the store that just declared them proves the
    // catalog was readable in that process; it proves nothing about the node
    // coming back up, which is the case an operator lives with.
    //
    // So the store is closed and reopened, and the runner is started against the
    // reopened one, with nothing but the catalog to go on.
    let backend = Arc::new(MemoryBackend::new()) as Arc<dyn KvBackend>;
    {
        let first = declared_on(&backend, "quarantine", "");
        // And nothing is running on it, so what happens below cannot be a
        // consumer that was already going.
        assert!(first.running().progress("orders_in").is_none());
    }

    let reopened = Store::open(Arc::clone(&backend)).unwrap();
    assert!(
        reopened.running().progress("orders_in").is_none(),
        "a reopened store claims to be running a consumer it has not started"
    );

    let broker: Arc<dyn Broker> = Arc::new(Handing {
        messages: vec![message(0, 1, r#"{"order_id": 7, "amount": 500}"#)],
        commit_fails: false,
        polls: Arc::new(AtomicUsize::new(0)),
        slow: Duration::ZERO,
        handed: Arc::new(AtomicBool::new(false)),
    });
    until(&reopened, broker, |store| !landed(store).is_empty());

    let records = landed(&reopened);
    assert_eq!(
        records.len(),
        1,
        "a restarted node did not start the consumer its catalog declares: {records:?}"
    );
}
