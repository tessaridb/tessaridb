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
use bgv_db_kv::{KvBackend, MemoryBackend};
use bgv_db_storage::Store;

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
    let (status, _, answered) = send(address, method, path, body, None);
    (status, answered)
}

/// One request carrying an `Authorization` value, and everything it answers
/// with — the headers included, because a `401` that omits its challenge is not
/// a `401` a client can act on.
fn send(
    address: &str,
    method: &str,
    path: &str,
    body: &str,
    credential: Option<&str>,
) -> (u16, Vec<String>, String) {
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

    let mut headers = Vec::new();
    loop {
        let mut line = String::new();
        reader.read_line(&mut line).unwrap();
        if line.trim().is_empty() {
            break;
        }
        headers.push(line.trim().to_owned());
    }
    let mut answered = String::new();
    reader.read_to_string(&mut answered).unwrap();
    (status, headers, answered)
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

#[test]
fn concurrent_requests_do_not_interfere() {
    // A session per request over one store is what the store already supports;
    // this asserts the server does not undo that. Writers race each other for
    // the committed tail, so some may lose and retry — what must not happen is
    // a lost write or a wrong read.
    let (_node, address) = node();
    request(&address, "POST", "/script", READY);

    let writers: Vec<_> = (0..8_u8)
        .map(|n| {
            let address = address.clone();
            std::thread::spawn(move || {
                request(
                    &address,
                    "POST",
                    "/script",
                    &format!(
                        "USE NAMESPACE prod DATABASE orders; \
                         CREATE users:{n} = {{ name: 'writer{n}' }};"
                    ),
                )
            })
        })
        .collect();

    let readers: Vec<_> = (0..4_u8)
        .map(|_| {
            let address = address.clone();
            std::thread::spawn(move || {
                request(
                    &address,
                    "POST",
                    "/script",
                    "USE NAMESPACE prod DATABASE orders; SELECT * FROM users;",
                )
            })
        })
        .collect();

    let mut written = 0_usize;
    for writer in writers {
        let (status, body) = writer.join().unwrap();
        // 200 for a write that landed; 409 for one that lost the race for the
        // committed tail, which is contention rather than corruption.
        assert!(status == 200 || status == 409, "{status} {body}");
        if status == 200 {
            written = written.saturating_add(1);
        }
    }
    for reader in readers {
        let (status, body) = reader.join().unwrap();
        assert_eq!(
            status, 200,
            "a read failed while writes were in flight: {body}"
        );
    }

    // Every write that answered 200 is there, and nothing else is.
    let (status, body) = request(
        &address,
        "POST",
        "/script",
        "USE NAMESPACE prod DATABASE orders; SELECT count(*) AS n FROM users;",
    );
    assert_eq!(status, 200, "{body}");
    assert!(
        body.contains(&format!(r#""n":{written}"#)),
        "expected {written} records, got {body}"
    );
}

// ------------------------------------------------------------------ identity

/// Credentials as a client sends them. Written out rather than computed, so a
/// change to the decoder cannot quietly agree with itself in both directions.
const ROOT: &str = "Basic cm9vdDpyb290IHNlY3JldA=="; // root:root secret
const GRACE: &str = "Basic Z3JhY2U6d2F0Y2ggb25seQ=="; // grace:watch only
const WRONG: &str = "Basic cm9vdDp3cm9uZw=="; // root:wrong

/// Every request is its own session, so every script says where it runs.
const IN_PROD: &str = "USE NAMESPACE prod; USE DATABASE orders; ";

/// A node whose store is closed, with an owner and a viewer.
fn closed() -> (Arc<Node>, String) {
    let (node, address) = node();
    let (status, _, body) = send(
        &address,
        "POST",
        "/script",
        "DEFINE NAMESPACE prod; USE NAMESPACE prod; \
         DEFINE DATABASE orders; USE DATABASE orders; \
         DEFINE TABLE notes; CREATE notes:1 = { body: 'x' }; \
         DEFINE USER root ROLE owner PASSWORD 'root secret';",
        None,
    );
    assert_eq!(status, 200, "{body}");
    let (status, _, body) = send(
        &address,
        "POST",
        "/script",
        "DEFINE USER grace ROLE viewer PASSWORD 'watch only';",
        Some(ROOT),
    );
    assert_eq!(status, 200, "{body}");
    (node, address)
}

#[test]
fn an_open_store_answers_a_request_that_carries_no_credential() {
    // Which is what keeps an empty store usable at all: requiring a signin
    // against one locks everybody out with no way in to fix it.
    let (_node, address) = node();
    let (status, body) = request(&address, "POST", "/script", READY);
    assert_eq!(status, 200, "{body}");
}

#[test]
fn a_closed_store_answers_401_with_a_challenge_and_403_when_the_role_forbids() {
    let (_node, address) = closed();

    // "I do not know you" — and a 401 without its challenge is not one a client
    // can act on, so the header is asserted rather than assumed.
    let (status, headers, body) = send(
        &address,
        "POST",
        "/script",
        &format!("{IN_PROD}SELECT * FROM notes;"),
        None,
    );
    assert_eq!(status, 401, "{body}");
    assert!(
        headers
            .iter()
            .any(|header| header.to_ascii_lowercase().starts_with("www-authenticate:")),
        "{headers:?}"
    );

    // A refused credential is the same answer as none: telling them apart tells
    // an attacker which half to keep guessing at.
    let (status, _, _) = send(
        &address,
        "POST",
        "/script",
        &format!("{IN_PROD}SELECT * FROM notes;"),
        Some(WRONG),
    );
    assert_eq!(status, 401);

    // "I know you, and no" — a different thing, and a client that cannot tell
    // retries a signin that will never help.
    let (status, _, body) = send(
        &address,
        "POST",
        "/script",
        &format!("{IN_PROD}SELECT * FROM notes;"),
        Some(GRACE),
    );
    assert_eq!(status, 200, "a viewer may read: {body}");
    let (status, _, body) = send(
        &address,
        "POST",
        "/script",
        &format!("{IN_PROD}CREATE notes:2 = {{ body: 'no' }};"),
        Some(GRACE),
    );
    assert_eq!(status, 403, "{body}");
}

#[test]
fn health_needs_no_credential_even_when_the_store_is_closed() {
    // A load balancer must not need one to tell a live node from a dead socket,
    // and the answer carries no data of anybody's.
    let (_node, address) = closed();
    let (status, _) = request(&address, "GET", "/health", "");
    assert_eq!(status, 200);
}

/// A backend that says a background failure has happened.
///
/// Everything is delegated to a real one, so the store under test behaves
/// exactly as it always does — the only difference is the number the health
/// check asks for. Written here rather than as a production seam, because a
/// store that can be *told* it is unwell is a store with a way to lie.
#[derive(Debug)]
struct Ailing {
    held: MemoryBackend,
}

impl KvBackend for Ailing {
    fn name(&self) -> &'static str {
        "ailing"
    }

    fn background_errors(&self) -> bgv_db_kv::Result<u64> {
        Ok(3)
    }

    fn get(
        &self,
        keyspace: bgv_db_kv::Keyspace,
        key: &bgv_db_kv::Key,
    ) -> bgv_db_kv::Result<Option<bgv_db_kv::Value>> {
        self.held.get(keyspace, key)
    }

    fn scan(
        &self,
        request: &bgv_db_kv::ScanRequest,
    ) -> bgv_db_kv::Result<Vec<(bgv_db_kv::Key, bgv_db_kv::Value)>> {
        self.held.scan(request)
    }

    fn apply(&self, batch: bgv_db_kv::WriteBatch) -> bgv_db_kv::Result<()> {
        self.held.apply(batch)
    }
}

#[test]
fn a_healthy_store_answers_health_with_two_hundred() {
    let (_node, address) = node();
    let (status, _, body) = send(&address, "GET", "/health", "", None);
    assert_eq!(status, 200);
    assert!(body.contains(r#""status":"ok""#), "{body}");
    assert!(body.contains(r#""committed""#), "{body}");
}

#[test]
fn a_store_with_a_background_failure_is_taken_out_of_rotation() {
    // The whole of the alerting design: an engine's compaction and flushing run
    // on their own threads, so a failure there surfaces at no call a caller
    // makes — the store answers reads while it has stopped keeping them. A 503
    // is what every load balancer and every monitor already act on, so the
    // alert is the one that exists rather than one written here and run never.
    let backend = Arc::new(Ailing {
        held: MemoryBackend::new(),
    }) as Arc<dyn KvBackend>;
    let db = Arc::new(Db::from_store(Store::open(backend).unwrap()));
    let node = Arc::new(Node::bind(db, "127.0.0.1:0").unwrap());
    let address = node.address();
    let serving = Arc::clone(&node);
    std::thread::spawn(move || serving.serve());

    let (status, _, body) = send(&address, "GET", "/health", "", None);
    assert_eq!(status, 503, "{body}");
    assert!(body.contains(r#""status":"unwell""#), "{body}");
    assert!(body.contains(r#""background_errors":3"#), "{body}");
    // And it says what is wrong, because a page reading only "unhealthy" sends
    // somebody to read code at three in the morning.
    assert!(body.contains("flush or compaction"), "{body}");
}
