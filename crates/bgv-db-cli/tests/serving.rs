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
