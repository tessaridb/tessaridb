//! The first user, declared from the environment on a node that came up alone.
//!
//! Exercised by starting real nodes, because the claim is about what a container
//! does on its way up. A unit test of the escaping proves the escaping; only a
//! running node proves that the store ends up **closed**, which is the whole
//! point.
//!
//! Each test gets its own store directory and its own port. The environment is
//! set per child process rather than per test process — `std::env::set_var` is
//! shared across threads and these tests run in parallel, so setting it here
//! would have each test racing every other for the same three variables.

#![allow(clippy::panic, clippy::unwrap_used)]

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};

/// The binary under test.
fn binary() -> PathBuf {
    let mut path = std::env::current_exe().unwrap();
    path.pop();
    if path.ends_with("deps") {
        path.pop();
    }
    path.join("tessaridb")
}

/// A running node that stops when it goes out of scope.
///
/// The guard is the point. A test that panics between spawning and killing
/// leaks a process holding a **fixed port**, and every later run of this file
/// then talks to a node from a previous run — which is not a failure that reads
/// like one: the store is stale, the sign-in is refused, and nothing says why.
/// This file's first draft did exactly that during an inversion check, so
/// cleanup runs on the unwinding path rather than on the happy one.
struct Node(Option<Child>);

impl Node {
    /// The child, for the tests that wait on it themselves.
    fn child(&mut self) -> &mut Child {
        self.0.as_mut().expect("a node that has not been taken")
    }

    /// What the node said on its error stream, after stopping it.
    fn stopped(&mut self) -> String {
        let Some(mut child) = self.0.take() else {
            return String::new();
        };
        drop(child.kill());
        let mut said = String::new();
        if let Some(errors) = child.stderr.as_mut() {
            drop(errors.read_to_string(&mut said));
        }
        drop(child.wait());
        said
    }
}

impl Drop for Node {
    fn drop(&mut self) {
        drop(self.stopped());
    }
}

/// A node serving HTTP on `port`, with `environment` set.
///
/// Ports are chosen per test rather than from a shared pool: the two fixed-port
/// suites in this workspace already collide with each other, and adding a third
/// would be repeating a known defect on purpose.
fn node(port: u16, store: &str, environment: &[(&str, &str)]) -> Node {
    let directory = std::env::temp_dir().join(format!("tessaridb-first-user-{store}"));
    drop(std::fs::remove_dir_all(&directory));
    let mut command = Command::new(binary());
    command
        .arg(&directory)
        .arg("--http")
        .arg(format!("127.0.0.1:{port}"))
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for (name, value) in environment {
        command.env(name, value);
    }
    Node(Some(command.spawn().expect("a node")))
}

/// Wait for the node to answer, or say what it printed instead of coming up.
fn awaited(node: &mut Node, port: u16) {
    let child = node.child();
    for _ in 0..200 {
        if TcpStream::connect(("127.0.0.1", port)).is_ok() {
            return;
        }
        if let Ok(Some(status)) = child.try_wait() {
            let mut said = String::new();
            if let Some(errors) = child.stderr.as_mut() {
                drop(errors.read_to_string(&mut said));
            }
            panic!("the node exited with {status} before answering: {said}");
        }
        std::thread::sleep(std::time::Duration::from_millis(25));
    }
    panic!("the node never answered on {port}");
}

/// One request, and the status and body it answers with.
fn send(port: u16, body: &str, credential: Option<&str>) -> (u16, String) {
    let address = format!("127.0.0.1:{port}");
    let mut stream = TcpStream::connect(&address).unwrap();
    let authorization =
        credential.map_or_else(String::new, |value| format!("Authorization: {value}\r\n"));
    let head = format!(
        "POST /script HTTP/1.1\r\nHost: {address}\r\n{authorization}Content-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    stream.write_all(head.as_bytes()).unwrap();
    stream.write_all(body.as_bytes()).unwrap();
    stream.flush().unwrap();

    let mut reader = BufReader::new(stream);
    let mut status_line = String::new();
    reader.read_line(&mut status_line).unwrap();
    let status: u16 = status_line
        .split_whitespace()
        .nth(1)
        .unwrap()
        .parse()
        .unwrap();
    loop {
        let mut line = String::new();
        reader.read_line(&mut line).unwrap();
        if line.trim().is_empty() {
            break;
        }
    }
    let mut answered = String::new();
    reader.read_to_string(&mut answered).unwrap();
    (status, answered)
}

/// Sign-in as a `Basic` header value.
fn basic(name: &str, password: &str) -> String {
    const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let raw = format!("{name}:{password}");
    let mut out = String::new();
    for chunk in raw.as_bytes().chunks(3) {
        let held = u32::from(chunk[0]) << 16
            | u32::from(chunk.get(1).copied().unwrap_or(0)) << 8
            | u32::from(chunk.get(2).copied().unwrap_or(0));
        for index in 0_u32..4 {
            if usize::try_from(index).unwrap() <= chunk.len() {
                let shift = 18_u32.saturating_sub(6_u32.saturating_mul(index));
                let sextet = usize::try_from((held >> shift) & 0x3f).unwrap();
                out.push(char::from(ALPHABET[sextet]));
            } else {
                out.push('=');
            }
        }
    }
    format!("Basic {out}")
}

#[test]
fn the_environment_declares_the_first_user_and_closes_the_store() {
    let port = 47931;
    let mut child = node(
        port,
        "closes",
        &[
            ("TESSARIDB_INITIAL_USER", "root"),
            ("TESSARIDB_INITIAL_PASSWORD", "correct horse battery"),
        ],
    );
    awaited(&mut child, port);

    // Closed: an anonymous caller gets nothing, which is the property that
    // matters and the one an unattended node cannot otherwise reach.
    let (status, body) = send(port, "INFO FOR STORE;", None);
    assert_eq!(status, 401, "the store should be closed: {body}");

    // And the declared user is a store-wide owner, so it can declare the users
    // that come after it and can take a backup.
    let (status, body) = send(
        port,
        "DEFINE NAMESPACE prod; INFO FOR USERS;",
        Some(&basic("root", "correct horse battery")),
    );
    assert_eq!(status, 200, "{body}");
}

#[test]
fn a_password_that_looks_like_a_statement_stays_a_password() {
    let port = 47932;
    // If this were concatenated rather than escaped, the quote would end the
    // literal and `mallory` would be declared alongside `root` — a second owner
    // nobody asked for, on a node that came up looking correct.
    let hostile = "'; DEFINE USER mallory ROLE owner PASSWORD 'a long enough one";
    let mut child = node(
        port,
        "hostile",
        &[
            ("TESSARIDB_INITIAL_USER", "root"),
            ("TESSARIDB_INITIAL_PASSWORD", hostile),
        ],
    );
    awaited(&mut child, port);

    // The whole hostile string is the password, quotes and all.
    let (status, body) = send(port, "INFO FOR STORE;", Some(&basic("root", hostile)));
    assert_eq!(
        status, 200,
        "the password should be the whole string: {body}"
    );

    // And nobody else was declared.
    let (status, body) = send(port, "INFO FOR USERS;", Some(&basic("root", hostile)));
    assert_eq!(status, 200, "{body}");
    assert!(
        !body.contains("mallory"),
        "a second owner was declared: {body}"
    );
}

#[test]
fn half_a_credential_stops_the_node_rather_than_opening_it() {
    let port = 47933;
    // A misspelled password variable would otherwise bring a node up **open**,
    // on a network, looking exactly like one that came up correctly.
    let mut child = node(port, "half", &[("TESSARIDB_INITIAL_USER", "root")]);
    let status = child.child().wait().expect("the node to exit");
    assert!(!status.success(), "a half credential must not start a node");

    let said = child.stopped();
    assert!(
        said.contains("TESSARIDB_INITIAL_PASSWORD"),
        "the refusal should name the variable that is missing: {said}"
    );
}

#[test]
fn a_name_that_could_carry_a_statement_stops_the_node() {
    let port = 47934;
    let mut child = node(
        port,
        "badname",
        &[
            ("TESSARIDB_INITIAL_USER", "root; DROP USER other"),
            ("TESSARIDB_INITIAL_PASSWORD", "correct horse battery"),
        ],
    );
    let status = child.child().wait().expect("the node to exit");
    assert!(!status.success(), "an unsafe name must not start a node");
}

#[test]
fn a_second_start_leaves_the_store_alone() {
    let port = 47935;
    let mut child = node(
        port,
        "again",
        &[
            ("TESSARIDB_INITIAL_USER", "root"),
            ("TESSARIDB_INITIAL_PASSWORD", "correct horse battery"),
        ],
    );
    awaited(&mut child, port);

    // The same environment, the same store, a different password. A container
    // restarts with whatever its environment holds, so this is the ordinary
    // case — and it must not be a way to reset a password, or the variables
    // would be a back door into every store that ever used them.
    let directory = std::env::temp_dir().join("tessaridb-first-user-again");
    let mut command = Command::new(binary());
    let mut child = Node(Some(
        command
            .arg(&directory)
            .arg("--http")
            .arg(format!("127.0.0.1:{port}"))
            .env("TESSARIDB_INITIAL_USER", "root")
            .env("TESSARIDB_INITIAL_PASSWORD", "a different horse entirely")
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("a node"),
    ));
    awaited(&mut child, port);

    let (status, body) = send(
        port,
        "INFO FOR STORE;",
        Some(&basic("root", "a different horse entirely")),
    );
    assert_eq!(
        status, 401,
        "the second start must not have reset it: {body}"
    );

    let (status, body) = send(
        port,
        "INFO FOR STORE;",
        Some(&basic("root", "correct horse battery")),
    );
    assert_eq!(
        status, 200,
        "the original password should still work: {body}"
    );
}
