//! What a caller over the wire sees.
//!
//! The routes are exercised through a real socket rather than by calling the
//! handlers, because the thing being tested is that a *client* can talk to this
//! — and a handler called directly proves only that the function works.

#![allow(clippy::panic, clippy::unwrap_used, clippy::indexing_slicing)]

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::sync::Arc;

use bgv_db::Db;
use bgv_db_http::Node;

/// A node on a loopback port the operating system picked, plus its address.
fn node() -> (Arc<Node>, String) {
    let db = Arc::new(Db::in_memory().unwrap());
    let node = Arc::new(Node::bind(db, "127.0.0.1:0").unwrap());
    let address = node.address();
    let serving = Arc::clone(&node);
    std::thread::spawn(move || serving.serve());
    (node, address)
}

/// One request, and the status and body it answers with.
fn request(address: &str, method: &str, path: &str, body: &str) -> (u16, String) {
    let mut stream = TcpStream::connect(address).unwrap();
    let head = format!(
        "{method} {path} HTTP/1.1\r\nHost: {address}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
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

    // Past the headers, then everything else is the body.
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

const READY: &str = "DEFINE NAMESPACE prod; USE NAMESPACE prod; \
                     DEFINE DATABASE orders; USE DATABASE orders; DEFINE TABLE users;";

#[test]
fn a_script_runs_over_the_wire_and_answers_one_object_per_statement() {
    let (_node, address) = node();
    let (status, body) = request(&address, "POST", "/script", READY);
    assert_eq!(status, 200, "{body}");
    // Five statements, five results.
    assert_eq!(body.matches(r#""kind":"done""#).count(), 5, "{body}");

    let (status, body) = request(
        &address,
        "POST",
        "/script",
        "USE NAMESPACE prod DATABASE orders; \
         CREATE users:1 = { name: 'ada' }; SELECT * FROM users:1;",
    );
    assert_eq!(status, 200, "{body}");
    assert!(body.contains(r#""kind":"records""#), "{body}");
    assert!(body.contains(r#""name":"ada""#), "{body}");
    // The access path is reported, so a scan is visible rather than folklore.
    assert!(body.contains(r#""path":"record""#), "{body}");
}

#[test]
fn the_status_says_what_kind_of_failure_it_was() {
    let (_node, address) = node();
    request(&address, "POST", "/script", READY);

    // The caller wrote it wrong: no amount of changing the data helps.
    let (status, body) = request(&address, "POST", "/script", "SELECT FROM;");
    assert_eq!(status, 400, "{body}");
    assert!(body.contains(r#""error""#), "{body}");

    // The caller wrote it right and the data says no: retriable after a change,
    // which is the whole reason this is not a 400.
    let (status, body) = request(
        &address,
        "POST",
        "/script",
        "USE NAMESPACE prod DATABASE orders; DEFINE TABLE users;",
    );
    assert_eq!(status, 409, "{body}");
    assert!(body.contains("already in use"), "{body}");
}

#[test]
fn health_says_whether_the_store_is_readable_and_not_only_that_a_socket_is_open() {
    let (_node, address) = node();
    let (status, body) = request(&address, "GET", "/health", "");
    assert_eq!(status, 200, "{body}");
    assert!(body.contains(r#""status":"ok""#), "{body}");
    assert!(body.contains(r#""committed":"#), "{body}");
}

#[test]
fn no_such_thing_and_not_that_way_are_different_answers() {
    let (_node, address) = node();
    let (status, _) = request(&address, "GET", "/nowhere", "");
    assert_eq!(status, 404);
    let (status, _) = request(&address, "GET", "/script", "");
    assert_eq!(status, 405);
}

#[test]
fn the_wire_and_the_library_answer_the_same_script() {
    // G002's C8, at the seam it is actually about: one script, two surfaces,
    // one answer.
    let db = Arc::new(Db::in_memory().unwrap());
    let node = Arc::new(Node::bind(Arc::clone(&db), "127.0.0.1:0").unwrap());
    let address = node.address();
    let serving = Arc::clone(&node);
    std::thread::spawn(move || serving.serve());

    let script = "DEFINE NAMESPACE prod; USE NAMESPACE prod; DEFINE DATABASE orders; \
                  USE DATABASE orders; DEFINE TABLE users; \
                  CREATE users:1 = { name: 'ada', city: 'Paris' };";
    let (status, _) = request(&address, "POST", "/script", script);
    assert_eq!(status, 200);

    // The same database, reached the other way.
    let mut session = db.session();
    session.run("USE NAMESPACE prod DATABASE orders;").unwrap();
    let embedded = session.run("SELECT name FROM users:1;").unwrap();
    assert_eq!(embedded[0].records().unwrap().len(), 1);

    let (status, body) = request(
        &address,
        "POST",
        "/script",
        "USE NAMESPACE prod DATABASE orders; SELECT name FROM users:1;",
    );
    assert_eq!(status, 200, "{body}");
    assert!(body.contains(r#""name":"ada""#), "{body}");
    // …and the projection dropped `city` on both surfaces.
    assert!(!body.contains("Paris"), "{body}");
}

#[test]
fn every_request_is_its_own_session() {
    // No cookies, no connection state, no `USE` that outlives a request — a
    // script says what it operates on.
    let (_node, address) = node();
    request(&address, "POST", "/script", READY);

    let (status, body) = request(&address, "POST", "/script", "SELECT * FROM users;");
    assert_eq!(status, 400, "{body}");
    assert!(body.contains("namespace"), "{body}");
}

#[test]
fn absent_and_null_are_told_apart_over_the_wire() {
    // JSON has one word for both, so the encoding uses JSON's own vocabulary:
    // a `value` key that is not there means `none`.
    let (_node, address) = node();
    request(
        &address,
        "POST",
        "/script",
        "DEFINE NAMESPACE prod; USE NAMESPACE prod; DEFINE DATABASE orders; \
         USE DATABASE orders; DEFINE SPACE cache; SET cache:'a' = NULL;",
    );

    let (_, body) = request(
        &address,
        "POST",
        "/script",
        "USE NAMESPACE prod DATABASE orders; GET cache:'a'; GET cache:'missing';",
    );
    assert!(body.contains(r#"{"kind":"value","value":null}"#), "{body}");
    assert!(body.contains(r#"{"kind":"value"}"#), "{body}");
}
