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
use tessari_storage::Now;

use tessari_http::Node;
use tessaridb::Db;

fn node() -> (Arc<Node>, String) {
    let db = Arc::new(Db::in_memory().now().unwrap());
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

/// One binary request: the remote-write body is snappy, not text.
fn send_bytes(address: &str, path: &str, body: &[u8]) -> (u16, String) {
    let mut stream = TcpStream::connect(address).unwrap();
    let head = format!(
        "POST {path} HTTP/1.1\r\nHost: {address}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    stream.write_all(head.as_bytes()).unwrap();
    stream.write_all(body).unwrap();
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
    let mut rest = String::new();
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).unwrap() == 0 {
            break;
        }
        rest.push_str(&line);
    }
    (status, rest)
}

/// A `WriteRequest` for one series of one sample, built by hand — the same
/// three messages the route's walker reads.
fn write_request(metric: &str, value: f64, at_ms: i64) -> Vec<u8> {
    fn varint(out: &mut Vec<u8>, mut held: u64) {
        loop {
            let byte = u8::try_from(held & 0x7f).unwrap_or(0);
            held >>= 7;
            if held == 0 {
                out.push(byte);
                return;
            }
            out.push(byte | 0x80);
        }
    }
    fn field(out: &mut Vec<u8>, number: u64, wire: u64) {
        varint(out, (number << 3) | wire);
    }
    fn text(out: &mut Vec<u8>, number: u64, held: &str) {
        field(out, number, 2);
        varint(out, held.len() as u64);
        out.extend_from_slice(held.as_bytes());
    }
    let mut label_name = Vec::new();
    text(&mut label_name, 1, "__name__");
    text(&mut label_name, 2, metric);
    let mut label_job = Vec::new();
    text(&mut label_job, 1, "job");
    text(&mut label_job, 2, "probe");
    let mut sample = Vec::new();
    field(&mut sample, 1, 1);
    sample.extend_from_slice(&value.to_le_bytes());
    field(&mut sample, 2, 0);
    varint(&mut sample, u64::try_from(at_ms).unwrap_or(0));
    let mut series = Vec::new();
    field(&mut series, 1, 2);
    varint(&mut series, label_name.len() as u64);
    series.extend_from_slice(&label_name);
    field(&mut series, 1, 2);
    varint(&mut series, label_job.len() as u64);
    series.extend_from_slice(&label_job);
    field(&mut series, 2, 2);
    varint(&mut series, sample.len() as u64);
    series.extend_from_slice(&sample);
    let mut request = Vec::new();
    field(&mut request, 1, 2);
    varint(&mut request, series.len() as u64);
    request.extend_from_slice(&series);
    request
}

#[test]
fn a_remote_write_batch_lands_with_its_labels_and_instants() {
    let (_node, address) = node();
    let (status, _) = send(
        &address,
        "POST",
        "/script",
        "DEFINE NAMESPACE remote; USE NAMESPACE remote; DEFINE DATABASE intake; USE DATABASE intake; DEFINE SERIES hits RETAIN 36500d TIME at;",
        None,
    );
    assert_eq!(status, 200);
    let plain = write_request("cpu_load", 0.75, 1_795_651_200_500);
    let packed = snap::raw::Encoder::new().compress_vec(&plain).unwrap();
    let (status, body) = send_bytes(&address, "/series/remote/intake/hits/remote-write", &packed);
    assert_eq!(status, 200, "{body}");
    assert!(body.contains(r#""appended":1"#), "{body}");
    let (status, body) = send(
        &address,
        "POST",
        "/script",
        "USE NAMESPACE remote; USE DATABASE intake; SELECT metric, job, value, at FROM hits;",
        None,
    );
    assert_eq!(status, 200);
    assert!(body.contains("cpu_load"), "{body}");
    assert!(body.contains("probe"), "{body}");
    assert!(body.contains("0.75"), "{body}");
    assert!(body.contains("2026-11-26"), "{body}");
}

#[test]
fn a_remote_write_refuses_what_it_cannot_land_honestly() {
    let (_node, address) = node();
    let (status, _) = send(
        &address,
        "POST",
        "/script",
        "DEFINE NAMESPACE remote; USE NAMESPACE remote; DEFINE DATABASE intake; USE DATABASE intake; DEFINE SERIES arrived RETAIN 36500d;",
        None,
    );
    assert_eq!(status, 200);
    let plain = write_request("m", 1.0, 1_000);
    let packed = snap::raw::Encoder::new().compress_vec(&plain).unwrap();
    // A series ordered by arrival time cannot hold the samples' instants.
    let (status, body) = send_bytes(
        &address,
        "/series/remote/intake/arrived/remote-write",
        &packed,
    );
    assert_eq!(status, 400, "{body}");
    assert!(body.contains("arrival time"), "{body}");
    // A body that is not snappy is refused whole.
    let (status, body) = send_bytes(
        &address,
        "/series/remote/intake/arrived/remote-write",
        b"not snappy",
    );
    assert_eq!(status, 400, "{body}");
    // A series that is not there is a 404, after the credential would have
    // been asked for on a secured node.
    let (status, _) = send_bytes(
        &address,
        "/series/remote/intake/nothing/remote-write",
        &packed,
    );
    assert_eq!(status, 404);
}

/// A `ReadRequest` for one query over one matcher, built to remote.proto's
/// field numbers — the same messages the route's walker reads.
fn read_request(start_ms: i64, end_ms: i64, name: &str, value: &str) -> Vec<u8> {
    read_request_of_kind(start_ms, end_ms, name, value, 0)
}

/// The same, with the matcher's kind chosen — 2 is the regular expression the
/// route refuses.
fn read_request_of_kind(start_ms: i64, end_ms: i64, name: &str, value: &str, kind: u64) -> Vec<u8> {
    fn varint(out: &mut Vec<u8>, mut held: u64) {
        loop {
            let byte = u8::try_from(held & 0x7f).unwrap_or(0);
            held >>= 7;
            if held == 0 {
                out.push(byte);
                return;
            }
            out.push(byte | 0x80);
        }
    }
    fn field(out: &mut Vec<u8>, number: u64, wire: u64) {
        varint(out, (number << 3) | wire);
    }
    fn text(out: &mut Vec<u8>, number: u64, held: &str) {
        field(out, number, 2);
        varint(out, u64::try_from(held.len()).unwrap_or(0));
        out.extend_from_slice(held.as_bytes());
    }
    let mut matcher = Vec::new();
    field(&mut matcher, 1, 0);
    varint(&mut matcher, kind);
    text(&mut matcher, 2, name);
    text(&mut matcher, 3, value);
    let mut query = Vec::new();
    field(&mut query, 1, 0);
    varint(&mut query, u64::try_from(start_ms).unwrap_or(0));
    field(&mut query, 2, 0);
    varint(&mut query, u64::try_from(end_ms).unwrap_or(0));
    field(&mut query, 3, 2);
    varint(&mut query, u64::try_from(matcher.len()).unwrap_or(0));
    query.extend_from_slice(&matcher);
    let mut request = Vec::new();
    field(&mut request, 1, 2);
    varint(&mut request, u64::try_from(query.len()).unwrap_or(0));
    request.extend_from_slice(&query);
    request
}

/// The response, walked far enough to name what it holds: metric names and
/// sample values.
fn read_response(bytes: &[u8]) -> String {
    let plain = snap::raw::Decoder::new().decompress_vec(bytes).unwrap();
    // A lossy walk is enough for the assertions: every label value and every
    // fixed64 sample value appears in the bytes.
    let mut named = String::new();
    for piece in plain.split(|byte| *byte == 0) {
        if let Ok(text) = std::str::from_utf8(piece) {
            named.push_str(text);
        }
        named.push(' ');
    }
    named
}

#[test]
fn a_remote_read_answers_what_a_remote_write_landed() {
    let (_node, address) = node();
    let (status, _) = send(
        &address,
        "POST",
        "/script",
        "DEFINE NAMESPACE remote; USE NAMESPACE remote; DEFINE DATABASE intake; USE DATABASE intake; DEFINE SERIES hits RETAIN 36500d TIME at;",
        None,
    );
    assert_eq!(status, 200);
    let plain = write_request("cpu_load", 0.75, 1_795_651_200_500);
    let packed = snap::raw::Encoder::new().compress_vec(&plain).unwrap();
    let (status, body) = send_bytes(&address, "/series/remote/intake/hits/remote-write", &packed);
    assert_eq!(status, 200, "{body}");
    // The round trip: the query names the metric, the range spans the sample.
    let query = read_request(1_795_651_200_000, 1_795_651_201_000, "__name__", "cpu_load");
    let packed = snap::raw::Encoder::new().compress_vec(&query).unwrap();
    let mut stream = TcpStream::connect(&address).unwrap();
    let head = format!(
        "POST /series/remote/intake/hits/remote-read HTTP/1.1\r\nHost: {address}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        packed.len()
    );
    stream.write_all(head.as_bytes()).unwrap();
    stream.write_all(&packed).unwrap();
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
    assert_eq!(status, 200);
    // Headers, then the binary body.
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).unwrap() == 0 || line == "\r\n" {
            break;
        }
    }
    let mut body = Vec::new();
    reader.read_to_end(&mut body).unwrap();
    let named = read_response(&body);
    assert!(named.contains("cpu_load"), "{named}");
    assert!(named.contains("probe"), "{named}");
}

#[test]
fn a_remote_read_refuses_what_it_cannot_answer_exactly() {
    let (_node, address) = node();
    let (status, _) = send(
        &address,
        "POST",
        "/script",
        "DEFINE NAMESPACE remote; USE NAMESPACE remote; DEFINE DATABASE intake; USE DATABASE intake; DEFINE SERIES hits RETAIN 36500d TIME at;",
        None,
    );
    assert_eq!(status, 200);
    // A regular-expression matcher: kind 2, refused rather than approximated.
    let query = read_request_of_kind(0, 1, "__name__", "cpu.*", 2);
    let packed = snap::raw::Encoder::new().compress_vec(&query).unwrap();
    let (status, body) = send_bytes(&address, "/series/remote/intake/hits/remote-read", &packed);
    assert_eq!(status, 400, "{body}");
    assert!(body.contains("regular-expression"), "{body}");
    // A body that is not snappy is refused whole.
    let (status, _) = send_bytes(
        &address,
        "/series/remote/intake/hits/remote-read",
        b"not snappy",
    );
    assert_eq!(status, 400);
    // A series that is not there is a 404.
    let empty = snap::raw::Encoder::new()
        .compress_vec(&read_request(0, 1, "__name__", "m"))
        .unwrap();
    let (status, _) = send_bytes(
        &address,
        "/series/remote/intake/missing/remote-read",
        &empty,
    );
    assert_eq!(status, 404);
}
