//! Appending events to a series over HTTP, through a real socket (G044 C12).
//!
//! `POST /series/{namespace}/{database}/{series}` with a TessariQL array of
//! objects: one transaction per batch, answered with how many landed. The route
//! is a surface over `CREATE`, so the cases that matter are the ones where that
//! could stop being true — a batch landing in part, a refusal answered as
//! success, and a second permission model.

#![allow(clippy::panic, clippy::unwrap_used, clippy::indexing_slicing)]

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::sync::Arc;

use tessari_http::Node;
use tessaridb::Db;

fn node() -> (Arc<Node>, String) {
    let db = Arc::new(Db::in_memory().unwrap());
    let node = Arc::new(Node::bind(db, "127.0.0.1:0").unwrap());
    let address = node.address();
    let serving = Arc::clone(&node);
    std::thread::spawn(move || crate::serve_until_the_test_ends(&serving));
    (node, address)
}

/// One request, and the status and body it answered with.
fn send(
    address: &str,
    method: &str,
    path: &str,
    body: &str,
    credential: Option<&str>,
) -> (u16, String) {
    let mut stream = TcpStream::connect(address).unwrap();
    let authorization =
        credential.map_or_else(String::new, |value| format!("Authorization: {value}\r\n"));
    let head = format!(
        "{method} {path} HTTP/1.1\r\nHost: {address}\r\n{authorization}Content-Length: {}\r\nConnection: close\r\n\r\n",
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

fn script(address: &str, source: &str, credential: Option<&str>) -> (u16, String) {
    send(address, "POST", "/script", source, credential)
}

const READY: &str = "DEFINE NAMESPACE prod; USE NAMESPACE prod; \
                     DEFINE DATABASE metrics; USE DATABASE metrics; \
                     DEFINE SERIES readings RETAIN 36500d TIME at; DEFINE TABLE plain SCHEMALESS;";

const ROUTE: &str = "/series/prod/metrics/readings";

fn event(second: u32, sensor: &str) -> String {
    format!("{{ sensor: '{sensor}', v: {second}, at: datetime '2026-09-29T10:00:{second:02}Z' }}")
}

fn stored(address: &str, credential: Option<&str>) -> String {
    let (status, body) = script(
        address,
        "USE NAMESPACE prod; USE DATABASE metrics; SELECT sensor, at FROM readings;",
        credential,
    );
    assert_eq!(status, 200, "{body}");
    body
}

#[test]
fn a_batch_lands_whole_in_event_time_order_and_says_how_many() {
    let (_node, address) = node();
    assert_eq!(script(&address, READY, None).0, 200);

    // Written late-first: the series is keyed by event time, so it reads back
    // early-first whatever order the batch carried.
    let batch = format!("[{}, {}, {}]", event(3, "c"), event(1, "a"), event(2, "b"));
    let (status, body) = send(&address, "POST", ROUTE, &batch, None);
    assert_eq!((status, body.as_str()), (200, r#"{"appended":3}"#));

    let answer = stored(&address, None);
    let (a, b, c) = (
        answer.find("\"a\"").unwrap(),
        answer.find("\"b\"").unwrap(),
        answer.find("\"c\"").unwrap(),
    );
    assert!(a < b && b < c, "event-time order: {answer}");
}

#[test]
fn one_event_the_series_refuses_lands_none_of_the_batch() {
    let (_node, address) = node();
    assert_eq!(script(&address, READY, None).0, 200);

    // The middle event has no `at`, which an event-time series refuses.
    let batch = format!(
        "[{}, {{ sensor: 'x', v: 0 }}, {}]",
        event(1, "a"),
        event(2, "b")
    );
    let (status, body) = send(&address, "POST", ROUTE, &batch, None);
    assert_eq!(status, 400, "{body}");
    assert!(!stored(&address, None).contains("\"a\""), "nothing landed");
}

#[test]
fn what_is_not_a_batch_of_events_for_a_series_is_refused_before_anything_runs() {
    let (_node, address) = node();
    assert_eq!(script(&address, READY, None).0, 200);
    let one = format!("[{}]", event(1, "a"));

    for (path, body, expected) in [
        // A name is grammar; one that is not an identifier never reaches a statement.
        ("/series/prod/metrics/read-ings", one.as_str(), 400),
        ("/series/prod/metrics", one.as_str(), 404),
        // Not a series, and not there at all, are both "no series here".
        ("/series/prod/metrics/plain", one.as_str(), 404),
        ("/series/prod/metrics/nothing", one.as_str(), 404),
        // The body is one value: an array of objects of literals.
        (ROUTE, "{ at: datetime '2026-09-29T10:00:00Z' }", 400),
        (ROUTE, "[1, 2]", 400),
        (ROUTE, "[{ at: time::now() }]", 400),
        (ROUTE, "[]; DROP TABLE readings", 400),
    ] {
        let (status, answered) = send(&address, "POST", path, body, None);
        assert_eq!(status, expected, "{path} {body}: {answered}");
    }
    assert_eq!(send(&address, "GET", ROUTE, "", None).0, 405);
    assert!(!stored(&address, None).contains("\"a\""), "nothing landed");
}

#[test]
fn a_closed_store_asks_for_a_credential_and_a_grant_like_every_route() {
    let (_node, address) = node();
    assert_eq!(script(&address, READY, None).0, 200);
    // owner:owner-pass and reader:reader-pass.
    let owner = Some("Basic b3duZXI6b3duZXItcGFzcw==");
    let reader = Some("Basic cmVhZGVyOnJlYWRlci1wYXNz");
    // The first user closes the store, so the second is declared by the first.
    assert_eq!(
        script(
            &address,
            "DEFINE USER owner ROLE owner PASSWORD 'owner-pass';",
            None
        )
        .0,
        200
    );
    assert_eq!(
        script(
            &address,
            "DEFINE USER reader ON prod.metrics ROLE viewer PASSWORD 'reader-pass';",
            owner,
        )
        .0,
        200
    );
    let batch = format!("[{}]", event(1, "a"));
    assert_eq!(send(&address, "POST", ROUTE, &batch, None).0, 401);
    assert_eq!(send(&address, "POST", ROUTE, &batch, reader).0, 403);
    assert_eq!(send(&address, "POST", ROUTE, &batch, owner).0, 200);
    assert!(stored(&address, owner).contains("\"a\""));
}
