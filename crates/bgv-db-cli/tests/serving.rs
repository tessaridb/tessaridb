//! The node as a **process**: started, written to over the wire, killed, and
//! reopened.
//!
//! # Why this is not covered by the tests that look like it
//!
//! Two other tests are close and neither is this one. The wire crate serves a
//! node *in-process*, so it never leaves an operating system's hands and nothing
//! is ever recovered. The storage crate kills a child that holds a store, which
//! proves durability but says nothing about a node — the process it kills is a
//! test binary, not the program somebody runs.
//!
//! What is asserted here is the join of the two: the shipped binary opens a
//! store, serves the protocol, is killed **uncatchably** while holding it, and a
//! second process reads back what the first acknowledged. That is what "the
//! server is a process" means, and none of the three properties is implied by
//! the other two.

#![allow(clippy::panic, clippy::unwrap_used)]

use std::net::TcpStream;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use bgv_db_wire::{Answer, Client};

/// The binary this crate builds, which is the one an operator installs.
const BGV: &str = env!("CARGO_BIN_EXE_bgv");

/// Wait for the node to accept connections, or say it never did.
///
/// Polled rather than slept on a guess: a fixed wait is either flaky on a loaded
/// machine or slow on an idle one, and this is neither.
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

/// Start the shipped binary serving `path` on `address`.
fn serving(path: &std::path::Path, address: &str) -> Child {
    let child = Command::new(BGV)
        .arg(path)
        .args(["--serve", address])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    assert!(
        listening(address, Duration::from_secs(20)),
        "the node never accepted a connection"
    );
    child
}

/// A running node that is killed when it goes out of scope, however it does.
///
/// A test that panics never reaches its own `kill`, and the child it started
/// keeps the fixed port — so the *next* run connects to the previous run's
/// node and fails for a reason that has nothing to do with what it asserts.
/// That cost an hour of the wrong diagnosis once ("the name ns:prod is already
/// in use", from a store this run never wrote), which is why it is a guard and
/// not a discipline.
struct Running(Child);

impl Drop for Running {
    fn drop(&mut self) {
        drop(self.0.kill());
        drop(self.0.wait());
    }
}

/// Start the shipped binary serving `path` on **both** surfaces.
fn serving_both(path: &std::path::Path, wire: &str, http: &str) -> Running {
    let child = Command::new(BGV)
        .arg(path)
        .args(["--serve", wire, "--http", http])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let running = Running(child);
    assert!(
        listening(wire, Duration::from_secs(20)),
        "the wire protocol never accepted a connection"
    );
    assert!(
        listening(http, Duration::from_secs(20)),
        "http never accepted a connection"
    );
    running
}

/// One HTTP request over a raw socket, answered whole.
///
/// Written out rather than taken from a client crate, for the reason the rest of
/// this program takes no dependency it can spell: a request is four lines of
/// text, and `Connection: close` makes the answer end at end-of-file so nothing
/// here has to parse a length.
fn over_http(address: &str, path: &str, body: &str) -> String {
    use std::io::{Read, Write};

    let mut socket = TcpStream::connect(address).unwrap();
    let request = format!(
        "POST {path} HTTP/1.1\r\nHost: {address}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    socket.write_all(request.as_bytes()).unwrap();
    socket.flush().unwrap();
    let mut answered = String::new();
    socket.read_to_string(&mut answered).unwrap();
    answered
}

#[test]
fn one_process_answers_on_both_surfaces_over_one_store() {
    // The property is not "two listeners started" — that is also what two
    // independent stores look like. It is that a write over one surface is
    // visible over the other, which only one store can produce.
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("store");
    // Fixed rather than zero for the reason the test below gives: the child
    // prints what it bound, and reading a pipe to learn it would make this test
    // depend on the banner's wording.
    let wire = "127.0.0.1:47831";
    let http = "127.0.0.1:47832";

    let node = serving_both(&path, wire, http);

    // Written over the wire protocol.
    {
        let mut client = Client::connect(wire).unwrap();
        client
            .run(
                "DEFINE NAMESPACE prod; USE NAMESPACE prod; \
                 DEFINE DATABASE orders; USE DATABASE orders; \
                 DEFINE TABLE users; CREATE users:1 = { who: 'ada' };",
                None,
            )
            .unwrap();
    }

    // Read over HTTP, from the same process.
    let answered = over_http(
        http,
        "/script",
        "USE NAMESPACE prod; USE DATABASE orders; SELECT * FROM users;",
    );
    // One assertion for one claim, deliberately. Split in two, the cheap status
    // check sits above the substantive one and catches every failure first —
    // and a claim nothing can reach is a claim nothing tests. The status is in
    // the message instead, where it diagnoses without gating.
    assert!(
        answered.starts_with("HTTP/1.1 200") && answered.contains("ada"),
        "http did not answer with the record the wire protocol wrote, \
         which is what one store behind two surfaces means: {answered}"
    );

    // And the health route, which needs no credential by design, so a failure
    // here is the surface being absent rather than a refusal.
    let alive = over_http(http, "/health", "");
    assert!(
        alive.starts_with("HTTP/1.1 200") || alive.starts_with("HTTP/1.1 405"),
        "http was not serving its own routes: {alive}"
    );

    drop(node);
}

#[test]
fn a_stopping_node_refuses_a_new_connection_and_finishes_the_store() {
    // The claim is *ordered*, not "the process ends" — a test that only asserted
    // termination would pass against `abort()`, which is the opposite of a
    // graceful shutdown. So: the port stops answering, and what was acknowledged
    // before the signal is still there when the store is reopened, which is what
    // "it closed the store rather than being killed mid-write" means.
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("store");
    let wire = "127.0.0.1:47841";
    let http = "127.0.0.1:47842";

    let node = serving_both(&path, wire, http);
    {
        let mut client = Client::connect(wire).unwrap();
        client
            .run(
                "DEFINE NAMESPACE prod; USE NAMESPACE prod; \
                 DEFINE DATABASE orders; USE DATABASE orders; \
                 DEFINE TABLE users; CREATE users:1 = { who: 'ada' };",
                None,
            )
            .unwrap();
    }

    // A real signal to a real child, which is the trigger an operator uses.
    // Sending one to this process instead would end the test harness.
    let signalled = Command::new("kill")
        .args(["-TERM", &node.0.id().to_string()])
        .status()
        .unwrap();
    assert!(signalled.success(), "the signal was not delivered");

    // Both ports stop answering. Polled rather than assumed immediate: the
    // stages run in order and the port closes when stage 1 reaches it, not when
    // the signal lands.
    assert!(
        stopped(wire, Duration::from_secs(20)),
        "the wire protocol went on accepting connections after being told to stop"
    );
    assert!(
        stopped(http, Duration::from_secs(20)),
        "http went on accepting connections after being told to stop"
    );

    // And it let go of the store rather than dying holding it: a second process
    // opens the same files and finds what the first acknowledged. A node killed
    // mid-shutdown would still pass the port checks above.
    drop(node);
    let reopened = serving_both(&path, wire, http);
    {
        let mut client = Client::connect(wire).unwrap();
        let answers = client
            .run(
                "USE NAMESPACE prod; USE DATABASE orders; SELECT * FROM users;",
                None,
            )
            .unwrap();
        let Answer::Records { records, .. } = &answers[2] else {
            panic!("not records: {:?}", answers[2]);
        };
        assert_eq!(records.len(), 1, "the store did not come back intact");
    }
    drop(reopened);
}

/// Wait for the node to stop accepting connections, or say it never did.
fn stopped(address: &str, patience: Duration) -> bool {
    let began = Instant::now();
    while began.elapsed() < patience {
        if TcpStream::connect(address).is_err() {
            return true;
        }
        std::thread::yield_now();
    }
    false
}

#[test]
fn the_node_is_a_process_that_survives_being_killed() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("store");
    // A port the operating system is not using. Fixed rather than zero, because
    // the child prints its address and reading a pipe to learn it would make the
    // test depend on the banner's wording.
    let address = "127.0.0.1:47823";

    let mut node = serving(&path, address);

    // Write through the protocol, and hold on to what the node acknowledged.
    {
        let mut client = Client::connect(address).unwrap();
        client
            .run(
                "DEFINE NAMESPACE prod; USE NAMESPACE prod; \
                 DEFINE DATABASE orders; USE DATABASE orders; \
                 DEFINE TABLE users;",
                None,
            )
            .unwrap();
        for held in 1..=50 {
            client
                .run(
                    &format!("CREATE users:{held} = {{ n: {held}, who: 'ada' }};"),
                    None,
                )
                .unwrap();
        }
        // Acknowledged means the node answered, which is the only thing a client
        // can observe and therefore the only thing worth asserting survives.
        let answers = client.run("SELECT * FROM users;", None).unwrap();
        let Answer::Records { records, .. } = &answers[0] else {
            panic!("not records: {:?}", answers[0]);
        };
        assert_eq!(records.len(), 50);
    }

    // Killed, not asked to stop. A node that only survives a graceful shutdown
    // has not been shown to survive anything.
    node.kill().unwrap();
    drop(node.wait());

    // A second process, over the same files.
    let mut reopened = serving(&path, address);
    {
        let mut client = Client::connect(address).unwrap();
        let answers = client
            .run(
                "USE NAMESPACE prod; USE DATABASE orders; SELECT * FROM users;",
                None,
            )
            .unwrap();
        let Answer::Records { records, .. } = &answers[2] else {
            panic!("not records: {:?}", answers[2]);
        };
        assert_eq!(
            records.len(),
            50,
            "the node came back holding fewer records than it acknowledged"
        );
    }
    reopened.kill().unwrap();
    drop(reopened.wait());
}

#[test]
fn a_second_node_will_not_open_a_store_another_one_holds() {
    // One writer, enforced by the engine's own lock rather than by anything this
    // program does — and worth pinning, because "two nodes on one directory" is
    // the mistake an operator makes once and cannot see the consequences of.
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("store");
    let address = "127.0.0.1:47824";

    let mut node = serving(&path, address);

    let second = Command::new(BGV)
        .arg(&path)
        .args(["--serve", "127.0.0.1:47825"])
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .output()
        .unwrap();
    assert!(!second.status.success(), "a second node opened the store");
    let said = String::from_utf8_lossy(&second.stderr);
    assert!(said.to_lowercase().contains("lock"), "{said}");

    node.kill().unwrap();
    drop(node.wait());
}

#[test]
fn the_binary_refuses_an_address_and_a_path_together() {
    // The argument rule, asserted against the shipped binary rather than against
    // the parser it happens to use.
    let mut refused = Command::new(BGV)
        .args(["./data", "--at", "127.0.0.1:1"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    drop(refused.stdin.take());
    let done = refused.wait_with_output().unwrap();
    assert!(!done.status.success());
    let said = String::from_utf8_lossy(&done.stderr);
    assert!(said.contains("two stores"), "{said}");
}
