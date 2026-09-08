//! The claim under **processes**, which is the only place it can be tested.
//!
//! # Why the queue's own tests do not cover this
//!
//! `tessari-session`'s fifteen queue cases all run inside one session, in one
//! thread. Every one of them asserts a property of the *statement*: the hold is
//! taken, the hold lapses, the attempt count is taken at the hand-out, the
//! ceiling refuses, a non-queue refuses. None of them can assert exclusivity,
//! because exclusivity is a property of two transactions racing and a single
//! thread cannot produce a race.
//!
//! What is asserted here is the join: the shipped binary is the node, the
//! consumers are separate operating-system processes competing over a real
//! socket, and one of them is killed **uncatchably while holding a claim**. The
//! three properties are that no record is handed to two of them, that no record
//! is lost, and that a record whose holder died comes back on its own.
//!
//! # The shape, and what it borrows
//!
//! The node half is `serving.rs`'s: the installed binary, a fixed port, and a
//! readiness probe polled to a deadline rather than waited on for a guessed
//! interval. The consumer half is `durability.rs`'s: the child is *this test
//! binary*, re-entered at an `#[ignore]`d test and told where the node is by an
//! environment variable, so a competing worker is a real process without a
//! second program having to exist to be one.
//!
//! Nothing here sleeps. A hold lapses at an instant the store wrote, so the way
//! to observe it is to ask again until the answer changes, bounded by a deadline
//! that reports what never happened.

#![allow(clippy::panic, clippy::unwrap_used)]

use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Write};
use std::net::TcpStream;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::time::{Duration, Instant};

use tessari_types::Value;
use tessari_wire::{Answer, Client};

/// The binary this crate builds, which is the one an operator installs.
const TESSARIDB: &str = env!("CARGO_BIN_EXE_tessaridb");

/// Environment variable carrying the node's address into a consumer child.
const NODE: &str = "TESSARIDB_QUEUE_BROKER_NODE";

/// How much work the producer seeds.
///
/// Large enough that four consumers overlap on it for long enough to race, and
/// small enough that the whole file stays a few seconds.
const JOBS: usize = 60;

/// How many competing consumer processes take part.
const CONSUMERS: usize = 4;

/// How many empty claims in a row a consumer takes as "the queue is drained".
///
/// A single empty answer is not evidence: a consumer can be handed nothing
/// because another consumer holds every claimable record at that instant, which
/// is the ordinary state of a contended queue rather than the end of the work.
const EMPTY_BEFORE_LEAVING: u32 = 200;

/// Where a child gives up on its own.
///
/// It exists only so a child cannot outlive a parent that died before ending it.
/// Every test here ends its children in well under a second.
const CHILD_GIVES_UP: Duration = Duration::from_secs(120);

/// How long a test waits for something it expects to happen.
const PATIENCE: Duration = Duration::from_secs(30);

/// Wait for the node to accept connections, or say it never did.
fn listening(address: &str, patience: Duration) -> bool {
    let began = Instant::now();
    while began.elapsed() < patience {
        if TcpStream::connect(address).is_ok() {
            return true;
        }
        std::thread::yield_now();
    }
    false
}

/// A running node that is killed when it goes out of scope, however it does.
///
/// The guard is `serving.rs`'s and it is here for the reason that file gives: a
/// test that panics never reaches its own `kill`, and the child it started keeps
/// the fixed port — so the *next* run connects to the previous run's node and
/// fails for a reason that has nothing to do with what it asserts.
struct Running(Child);

impl Drop for Running {
    fn drop(&mut self) {
        drop(self.0.kill());
        drop(self.0.wait());
    }
}

/// Start the shipped binary serving `path` on `address`.
fn serving(path: &std::path::Path, address: &str) -> Running {
    let child = Command::new(TESSARIDB)
        .arg(path)
        .args(["--serve", address])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let running = Running(child);
    assert!(
        listening(address, PATIENCE),
        "the node never accepted a connection"
    );
    running
}

/// A connection with the queue's namespace and database already selected.
fn worker(address: &str) -> Client {
    let mut client = Client::connect(address).unwrap();
    client
        .run("USE NAMESPACE prod; USE DATABASE work;", None)
        .unwrap();
    client
}

/// The identities one answer handed out, in the order it handed them.
fn handed_out(answers: &[Answer]) -> Vec<String> {
    match answers.first() {
        Some(Answer::Records { records, .. }) => records.iter().map(|(id, _)| id.clone()).collect(),
        other => panic!("a claim answered with something other than records: {other:?}"),
    }
}

/// Say something the parent will read, and make sure it can.
///
/// The flush is not decoration: the harness buffers stdout, and an announcement
/// the parent never reads proves nothing.
fn announce(line: &str) {
    println!("{line}");
    std::io::stdout().flush().unwrap();
}

/// Spawn one child of this binary at `test`, pointed at `address`.
///
/// Its standard input is a pipe nobody writes to, which is how a child that must
/// stay alive stays alive without spending a core doing it: a blocking read on a
/// pipe whose write end the parent holds returns when the parent kills it, and
/// never before.
fn child_at(test: &str, address: &str) -> Child {
    Command::new(std::env::current_exe().unwrap())
        .args(["--exact", test, "--ignored", "--nocapture"])
        .env(NODE, address)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap()
}

/// Read from a child until it announces something, or say it never did.
fn announcement(child: &mut Child, prefix: &str) -> String {
    let stdout = child.stdout.take().unwrap();
    let began = Instant::now();
    for line in BufReader::new(stdout).lines() {
        let line = line.unwrap();
        if let Some(rest) = line.strip_prefix(prefix) {
            return rest.trim().to_owned();
        }
        assert!(
            began.elapsed() < PATIENCE,
            "the child never said {prefix:?}"
        );
    }
    panic!("the child ended without saying {prefix:?}")
}

/// The address a child was told to work against.
fn node_address() -> String {
    std::env::var(NODE).unwrap_or_else(|_| panic!("{NODE} must name the node"))
}

/// One of the competing consumers: claim, finish, repeat until nothing is left.
///
/// It is `#[ignore]`d because it is not a test — it is the other half of one,
/// and it only has a node to talk to when a parent gives it one.
#[test]
#[ignore = "spawned by the broker test as one of the competing consumers"]
fn drain_the_queue() {
    let address = node_address();
    let mut client = worker(&address);

    let began = Instant::now();
    let mut empty = 0_u32;
    while began.elapsed() < CHILD_GIVES_UP && empty < EMPTY_BEFORE_LEAVING {
        match client.run("CLAIM FROM jobs;", None) {
            Ok(answers) => {
                let taken = handed_out(&answers);
                if taken.is_empty() {
                    empty = empty.saturating_add(1);
                    std::thread::yield_now();
                    continue;
                }
                empty = 0;
                for id in taken {
                    // Announced **before** the record is finished, so a claim
                    // this consumer never completes is still on the record the
                    // parent judges. An announcement after the delete would
                    // hide exactly the case worth catching.
                    announce(&format!("claimed {id}"));
                    client.run(&format!("DELETE jobs:{id};"), None).unwrap();
                    announce(&format!("finished {id}"));
                }
            }
            // A losing worker is not a broken one. What the loss *is* — a
            // refusal here, or a record taken on a retry nobody sees — is what
            // the parent reads these lines to find out.
            Err(why) => announce(&format!("lost {why}")),
        }
    }
}

/// A consumer that takes one claim and then stops, still holding it.
#[test]
#[ignore = "spawned by the redelivery tests; it holds a claim and waits to be killed"]
fn claim_and_hang() {
    let address = node_address();
    let mut client = worker(&address);

    let began = Instant::now();
    loop {
        assert!(
            began.elapsed() < CHILD_GIVES_UP,
            "nothing was ever claimable"
        );
        if let Ok(answers) = client.run("CLAIM FROM jobs;", None) {
            if let Some(id) = handed_out(&answers).first() {
                announce(&format!("claimed {id}"));
                break;
            }
        }
        std::thread::yield_now();
    }

    // Holding, and doing nothing about it. Nothing is deleted and nothing is
    // released; the parent ends this process while it is here, which is what
    // "the claimant died" means to the store.
    let mut ignored = String::new();
    drop(std::io::stdin().read_line(&mut ignored));
}

#[test]
fn competing_processes_drain_a_queue_and_no_record_is_handed_out_twice() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("store");
    let address = "127.0.0.1:47871";
    let node = serving(&path, address);

    // The producer. A thirty-second hold is deliberately far longer than the
    // run: no hold can lapse inside this test, so a record appearing twice is a
    // record handed out twice and never a redelivery wearing its clothes.
    {
        let mut client = Client::connect(address).unwrap();
        client
            .run(
                "DEFINE NAMESPACE prod; USE NAMESPACE prod; \
                 DEFINE DATABASE work; USE DATABASE work; \
                 DEFINE QUEUE jobs TIMEOUT 30s;",
                None,
            )
            .unwrap();
        let mut seeding = String::new();
        for n in 1..=JOBS {
            seeding.push_str(&format!("CREATE jobs:{n} = {{ url: 'j{n}' }};"));
        }
        client.run(&seeding, None).unwrap();
    }

    let mut consumers: Vec<Child> = (0..CONSUMERS)
        .map(|_| child_at("drain_the_queue", address))
        .collect();

    let mut claimed: Vec<String> = Vec::new();
    let mut finished: Vec<String> = Vec::new();
    let mut lost: Vec<String> = Vec::new();
    for consumer in &mut consumers {
        let stdout = consumer.stdout.take().unwrap();
        for line in BufReader::new(stdout).lines() {
            let line = line.unwrap();
            if let Some(id) = line.strip_prefix("claimed ") {
                claimed.push(id.trim().to_owned());
            } else if let Some(id) = line.strip_prefix("finished ") {
                finished.push(id.trim().to_owned());
            } else if let Some(why) = line.strip_prefix("lost ") {
                lost.push(why.trim().to_owned());
            }
        }
        drop(consumer.wait());
    }

    // The claim this whole file exists for. Two live claimants holding one
    // record is not directly observable from outside, so it is observed by its
    // only consequence: the same identity handed to two of them.
    let mut times_handed: BTreeMap<&str, usize> = BTreeMap::new();
    for id in &claimed {
        let count = times_handed.entry(id.as_str()).or_insert(0);
        *count = count.saturating_add(1);
    }
    let twice: Vec<&&str> = times_handed
        .iter()
        .filter(|(_, count)| **count > 1)
        .map(|(id, _)| id)
        .collect();
    assert!(
        twice.is_empty(),
        "records handed to more than one consumer while nothing could lapse: \
         {twice:?}"
    );

    // And nothing was lost, which is the other half and does not follow from
    // the first: a queue that hands nothing out twice by handing some of it out
    // never would pass the assertion above.
    assert_eq!(
        times_handed.len(),
        JOBS,
        "the queue was drained of {} of {JOBS} records",
        times_handed.len()
    );
    assert_eq!(
        finished.len(),
        JOBS,
        "{} of {JOBS} records were claimed and never finished",
        finished.len()
    );

    // Read from the node rather than from the children, because the children
    // are reporting on themselves. An empty table is the store's own account of
    // the same run.
    {
        let mut client = worker(address);
        let answers = client.run("SELECT * FROM jobs;", None).unwrap();
        assert!(
            handed_out(&answers).is_empty(),
            "the consumers reported finishing every record and the store still \
             holds some"
        );
    }

    // Printed rather than asserted: contention is near-certain with four
    // consumers on a strictly ordered queue, but it is not guaranteed, and a
    // test that failed on a fast enough machine would be asserting the
    // scheduler. What the losses look like is the evidence this run carries.
    println!(
        "[broker] {} hand-outs, {} finished, {} lost races; first loss: {}",
        claimed.len(),
        finished.len(),
        lost.len(),
        lost.first().map_or("none", String::as_str)
    );

    drop(node);
}

#[test]
fn a_record_a_live_claimant_holds_is_offered_to_nobody_else() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("store");
    let address = "127.0.0.1:47872";
    let node = serving(&path, address);

    {
        let mut client = Client::connect(address).unwrap();
        client
            .run(
                "DEFINE NAMESPACE prod; USE NAMESPACE prod; \
                 DEFINE DATABASE work; USE DATABASE work; \
                 DEFINE QUEUE jobs TIMEOUT 30s; \
                 CREATE jobs:1 = { url: 'only' };",
                None,
            )
            .unwrap();
    }

    let mut holder = child_at("claim_and_hang", address);
    // Kept, deliberately. Dropping the write end closes the child's standard
    // input, which is what it is blocked on — so a dropped handle would end the
    // holder early and this test would assert against a corpse.
    let holding: Option<ChildStdin> = holder.stdin.take();
    assert_eq!(announcement(&mut holder, "claimed "), "1");

    // Thirty seconds of hold against the milliseconds this takes: if the record
    // comes back here, it came back because it was never held.
    let mut client = worker(address);
    let answers = client.run("CLAIM FROM jobs;", None).unwrap();
    assert!(
        handed_out(&answers).is_empty(),
        "a record another process is holding was handed out to a second claimant"
    );

    drop(holding);
    drop(holder.kill());
    drop(holder.wait());
    drop(node);
}

#[test]
fn a_claim_its_holder_died_holding_is_handed_out_again() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("store");
    let address = "127.0.0.1:47873";
    let node = serving(&path, address);

    // A one-second hold, so the lapse is observable inside a test. The length is
    // the only thing scaled down: the comparison a reader makes against a stored
    // deadline is the same one a thirty-second hold reaches thirty seconds later.
    {
        let mut client = Client::connect(address).unwrap();
        client
            .run(
                "DEFINE NAMESPACE prod; USE NAMESPACE prod; \
                 DEFINE DATABASE work; USE DATABASE work; \
                 DEFINE QUEUE jobs TIMEOUT 1s; \
                 CREATE jobs:1 = { url: 'only' };",
                None,
            )
            .unwrap();
    }

    let mut holder = child_at("claim_and_hang", address);
    let holding: Option<ChildStdin> = holder.stdin.take();
    assert_eq!(announcement(&mut holder, "claimed "), "1");

    // Killed, not asked to stop. No destructor runs, no release is sent, and
    // nothing tells the store anything happened — which is the case the design
    // says needs no liveness detection to recover from.
    holder.kill().unwrap();
    drop(holder.wait());
    drop(holding);

    let mut client = worker(address);
    let began = Instant::now();
    let returned = loop {
        assert!(
            began.elapsed() < PATIENCE,
            "the record its holder died holding was never handed out again"
        );
        let answers = client.run("CLAIM FROM jobs;", None).unwrap();
        let Some(Answer::Records { records, .. }) = answers.first() else {
            panic!("a claim answered with something other than records");
        };
        if let Some((id, value)) = records.first() {
            assert_eq!(id, "1");
            break value.clone();
        }
        std::thread::yield_now();
    };

    // Handed out twice and counted twice. The count is what tells a dead-letter
    // predicate that this record has been tried before, and it is taken at the
    // hand-out precisely so that a claimant that died still spent an attempt.
    let Value::Object(fields) = &returned else {
        panic!("a queue record came back as something other than an object");
    };
    assert_eq!(
        fields.get("attempts"),
        Some(&Value::from(2_i64)),
        "the redelivery did not count as a second attempt"
    );

    drop(node);
}
