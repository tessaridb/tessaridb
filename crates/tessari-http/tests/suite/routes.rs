//! What a caller over the wire sees.
//!
//! The routes are exercised through a real socket rather than by calling the
//! handlers, because the thing being tested is that a *client* can talk to this
//! — and a handler called directly proves only that the function works.

#![allow(clippy::panic, clippy::unwrap_used, clippy::indexing_slicing)]

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::sync::Arc;

use tessari_constants::HTTP_MAX_BODY_BYTES;
use tessari_http::Node;
use tessari_kv::{KvBackend, MemoryBackend};
use tessari_storage::Store;
use tessaridb::Db;

/// A node on a loopback port the operating system picked, plus its address.
fn node() -> (Arc<Node>, String) {
    let db = Arc::new(Db::in_memory().unwrap());
    let node = Arc::new(Node::bind(db, "127.0.0.1:0").unwrap());
    let address = node.address();
    let serving = Arc::clone(&node);
    std::thread::spawn(move || crate::serve_until_the_test_ends(&serving));
    (node, address)
}

/// One request whose answer is **bytes** rather than text.
///
/// Its own helper because a backup is a binary file: reading it into a `String`
/// would either fail or lie about what came back, and the file is exactly what
/// this route exists to hand over.
fn send_bytes(
    address: &str,
    method: &str,
    path: &str,
    credential: Option<&str>,
) -> (u16, Vec<String>, Vec<u8>) {
    let mut stream = TcpStream::connect(address).unwrap();
    let authorization =
        credential.map_or_else(String::new, |value| format!("Authorization: {value}\r\n"));
    let head = format!(
        "{method} {path} HTTP/1.1\r\nHost: {address}\r\n{authorization}Content-Length: 0\r\nConnection: close\r\n\r\n"
    );
    stream.write_all(head.as_bytes()).unwrap();
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
    let mut held = Vec::new();
    reader.read_to_end(&mut held).unwrap();
    (status, headers, held)
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
                     DEFINE DATABASE orders; USE DATABASE orders; DEFINE COLLECTION users;";

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
    // …and a read with nothing to report says nothing, so every response that
    // had no note is byte-identical to what it was before notes existed.
    assert!(!body.contains(r#""notes""#), "{body}");
}

#[test]
fn a_note_reaches_the_client_over_http() {
    // The channel is only worth building if it arrives somewhere. This is the
    // surface it arrives on: a JSON key that is absent when there is nothing to
    // say, which every JSON reader already handles, and present with a kind a
    // client can group on and a message a person can act on.
    let (_node, address) = node();
    request(&address, "POST", "/script", READY);
    let (status, body) = request(
        &address,
        "POST",
        "/script",
        "USE NAMESPACE prod DATABASE orders; \
         CREATE users:1 = { name: 'ada' }; CREATE users:2 = { name: 'grace' }; \
         SELECT * FROM (SELECT * FROM users LIMIT 1);",
    );
    assert_eq!(status, 200, "{body}");
    assert!(body.contains(r#""kind":"subquery-ceiling""#), "{body}");
    assert!(body.contains("reached its ceiling of 1"), "{body}");
    // The note did not displace the answer it is about.
    assert!(body.contains(r#""kind":"records""#), "{body}");
    assert!(body.contains(r#""name":"ada""#), "{body}");
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
        "USE NAMESPACE prod DATABASE orders; DEFINE COLLECTION users;",
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
    std::thread::spawn(move || crate::serve_until_the_test_ends(&serving));

    let script = "DEFINE NAMESPACE prod; USE NAMESPACE prod; DEFINE DATABASE orders; \
                  USE DATABASE orders; DEFINE COLLECTION users; \
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
         DEFINE COLLECTION notes; CREATE notes:1 = { body: 'x' }; \
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

    fn background_errors(&self) -> tessari_kv::Result<u64> {
        Ok(3)
    }

    fn get(
        &self,
        keyspace: tessari_kv::Keyspace,
        key: &tessari_kv::Key,
    ) -> tessari_kv::Result<Option<tessari_kv::Value>> {
        self.held.get(keyspace, key)
    }

    fn scan(
        &self,
        request: &tessari_kv::ScanRequest,
    ) -> tessari_kv::Result<Vec<(tessari_kv::Key, tessari_kv::Value)>> {
        self.held.scan(request)
    }

    fn apply(&self, batch: tessari_kv::WriteBatch) -> tessari_kv::Result<()> {
        self.held.apply(batch)
    }
}

/// A backend whose first health question panics, and which answers after it.
#[derive(Debug)]
struct PanicsOnce {
    held: MemoryBackend,
    panicked: std::sync::atomic::AtomicBool,
}

impl KvBackend for PanicsOnce {
    fn name(&self) -> &'static str {
        "panics-once"
    }

    fn background_errors(&self) -> tessari_kv::Result<u64> {
        if !self
            .panicked
            .swap(true, std::sync::atomic::Ordering::SeqCst)
        {
            // `resume_unwind` unwinds exactly as a panic does, without the hook's noise.
            std::panic::resume_unwind(Box::new("a request that fails inside the node"));
        }
        Ok(0)
    }

    fn get(
        &self,
        keyspace: tessari_kv::Keyspace,
        key: &tessari_kv::Key,
    ) -> tessari_kv::Result<Option<tessari_kv::Value>> {
        self.held.get(keyspace, key)
    }

    fn scan(
        &self,
        request: &tessari_kv::ScanRequest,
    ) -> tessari_kv::Result<Vec<(tessari_kv::Key, tessari_kv::Value)>> {
        self.held.scan(request)
    }

    fn apply(&self, batch: tessari_kv::WriteBatch) -> tessari_kv::Result<()> {
        self.held.apply(batch)
    }
}

#[test]
fn a_request_that_panics_is_answered_and_the_node_answers_the_next_one() {
    // One request's panic takes that request down and nothing else: it is told
    // so in the node's words, and the listener goes on answering (H15).
    let backend = Arc::new(PanicsOnce {
        held: MemoryBackend::new(),
        panicked: std::sync::atomic::AtomicBool::new(false),
    }) as Arc<dyn KvBackend>;
    let db = Arc::new(Db::from_store(Store::open(backend).unwrap()));
    let node = Arc::new(Node::bind(db, "127.0.0.1:0").unwrap());
    let address = node.address();
    let serving = Arc::clone(&node);
    std::thread::spawn(move || crate::serve_until_the_test_ends(&serving));

    let (status, _, body) = send(&address, "GET", "/health", "", None);
    assert_eq!(status, 500, "{body}");
    assert!(body.contains("failed inside the node"), "{body}");
    let (status, _, body) = send(&address, "GET", "/health", "", None);
    assert_eq!(
        status, 200,
        "the node stopped answering after one panic: {body}"
    );
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
fn a_leaving_node_is_not_ready_while_it_is_still_answering() {
    // The property the whole readiness route exists for, and it is not "the
    // route replies": it is that the answer CHANGES while the node is still
    // reachable. A node that stopped accepting at the same moment would answer
    // this probe with a refused connection, which tells a load balancer to
    // retry rather than to route elsewhere.
    let (node, address) = node();

    let (status, _, body) = send(&address, "GET", "/ready", "", None);
    assert_eq!(status, 200, "{body}");
    assert!(body.contains(r#""status":"ok""#), "{body}");

    // Stage 0 only. Nothing is refused, so the next request is served.
    node.stopping().leaving();

    let (status, _, body) = send(&address, "GET", "/ready", "", None);
    assert_eq!(
        status, 503,
        "a leaving node still told a load balancer to send it work: {body}"
    );
    assert!(body.contains(r#""status":"leaving""#), "{body}");

    // And liveness is unmoved, because restarting a node that is shutting down
    // on purpose is the one thing a supervisor must not do here.
    let (status, _, body) = send(&address, "GET", "/health", "", None);
    assert_eq!(status, 200, "{body}");
}

#[test]
fn the_metrics_move_and_are_the_stores_own_numbers() {
    // A metrics route is the easiest thing here to test vacuously: six lines of
    // zeros pass any check that the route replies. So two claims, neither of
    // which a constant satisfies — the counter **rises** across two scrapes, and
    // the committed sequence is the same number `/health` reports.
    let (_node, address) = node();

    let (status, headers, first) = send(&address, "GET", "/metrics", "", None);
    assert_eq!(status, 200, "{first}");
    assert!(
        headers
            .iter()
            .any(|header| header.to_ascii_lowercase().contains("version=0.0.4")),
        "the exposition format's version was not declared: {headers:?}"
    );
    assert!(
        first.contains("# TYPE tessari_answers_total counter"),
        "a scrape that does not describe itself sends a dashboard author to read \
         source: {first}"
    );

    let before = counter(&first, "tessari_answers_total{surface=\"http\"}");

    // One request that is not a scrape, so the rise cannot be the scrape itself.
    let (_, _, _) = send(&address, "GET", "/health", "", None);

    let (_, _, second) = send(&address, "GET", "/metrics", "", None);
    let after = counter(&second, "tessari_answers_total{surface=\"http\"}");
    assert!(
        after > before,
        "the answer counter did not move, so it reports a constant rather than \
         this node: {before} then {after}"
    );

    // And the numbers are the store's rather than this route's own idea of them.
    //
    // The write is not decoration. An untouched store's committed sequence is
    // **zero**, so against that fixture "equals the store" and "equals the
    // literal 0" are the same assertion — and pinning the metric to a constant
    // passes. Found by injecting exactly that, which is why the precondition is
    // asserted below rather than assumed.
    let (_, _, _) = send(&address, "POST", "/script", "DEFINE NAMESPACE prod;", None);

    let (_, _, third) = send(&address, "GET", "/metrics", "", None);
    let (_, _, health) = send(&address, "GET", "/health", "", None);
    let committed = counter(&third, "tessari_committed_sequence");
    assert!(
        committed > 0,
        "nothing was committed, so this comparison cannot tell the store's \
         number from a constant: {third}"
    );
    assert!(
        health.contains(&format!(r#""committed":{committed}"#)),
        "the scraped sequence and the health route disagree about one store: \
         {committed} against {health}"
    );
}

#[test]
fn a_node_with_no_census_reports_itself_and_claims_no_uptime() {
    // Absent beats wrong. This node has no process around it enumerating
    // surfaces, so the only clock it could call uptime is its own bind time —
    // and one metric name meaning two things by how the node was started is
    // worse for a scraper than a series that is simply missing.
    let (_node, address) = node();
    let (status, _, body) = send(&address, "GET", "/metrics", "", None);
    assert_eq!(status, 200);
    assert!(
        !body.contains("tessari_uptime_seconds"),
        "a node with no process behind it reported a process uptime: {body}"
    );
    assert!(
        body.contains("tessari_connections{surface=\"http\"}"),
        "it should still report the counters it genuinely has: {body}"
    );
}

/// One metric's value out of a scrape, by its full name and labels.
fn counter(scrape: &str, name: &str) -> u64 {
    scrape
        .lines()
        .find_map(|line| line.strip_prefix(name))
        .unwrap_or_else(|| panic!("no {name} in the scrape:\n{scrape}"))
        .trim()
        .parse()
        .unwrap()
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
    std::thread::spawn(move || crate::serve_until_the_test_ends(&serving));

    let (status, _, body) = send(&address, "GET", "/health", "", None);
    assert_eq!(status, 503, "{body}");
    assert!(body.contains(r#""status":"unwell""#), "{body}");
    assert!(body.contains(r#""background_errors":3"#), "{body}");
    // And it says what is wrong, because a page reading only "unhealthy" sends
    // somebody to read code at three in the morning.
    assert!(body.contains("flush or compaction"), "{body}");
}

#[test]
fn a_record_reference_comes_back_as_something_a_client_can_follow() {
    // Before this, a reference rendered as `"1:2"` — the table's **id** where
    // its name belongs — which is indistinguishable from a reference a client
    // could use and is not one. The same defect was in the console, and one
    // resolver serves both, because two would eventually disagree about a table
    // that had been renamed.
    let (_node, address) = node();
    let script = "DEFINE NAMESPACE prod; USE NAMESPACE prod; DEFINE DATABASE orders; \
                  USE DATABASE orders; DEFINE COLLECTION users; DEFINE COLLECTION posts; \
                  CREATE users:1 = { name: 'ada' }; \
                  CREATE posts:1 = { author: users:1, tags: [users:1] }; \
                  SELECT * FROM posts:1;";
    let (status, body) = request(&address, "POST", "/script", script);
    assert_eq!(status, 200, "{body}");
    assert!(body.contains(r#""author":"users:1""#), "{body}");
    // And inside an array, because a reference can be anywhere in a record and a
    // resolver that only walked the top level would miss the interesting shapes.
    assert!(body.contains(r#"["users:1"]"#), "{body}");
    assert!(
        !body.contains(r#""1:1""#),
        "an id leaked into the answer: {body}"
    );
}

#[test]
fn the_backup_route_hands_over_a_file_the_verifier_reads() {
    // The route is a surface over the `BACKUP` statement (ADR-0011 §6), so what
    // it hands over must be the file `backup::write` produces — not a rendering
    // of one. The verifier is the check that says so, because it is the code an
    // operator would actually run against what they downloaded.
    let (_node, address) = node();
    let (status, body) = request(&address, "POST", "/script", READY);
    assert_eq!(status, 200, "{body}");

    let (status, headers, held) = send_bytes(&address, "GET", "/backup", None);
    assert_eq!(status, 200);
    assert!(
        headers.iter().any(|line| line
            .to_ascii_lowercase()
            .contains("application/octet-stream")),
        "{headers:?}"
    );
    // A snapshot unless the caller asks for the log (ADR-0094 D1), with its
    // length declared and never chunked (protocol §5.3) — it is spooled to disk
    // rather than built in memory (D6), and the length is the file's exact one.
    tessari_backup::verify_state(&mut held.as_slice()).unwrap();
    assert!(
        headers
            .iter()
            .any(|line| line.eq_ignore_ascii_case(&format!("content-length: {}", held.len()))),
        "the snapshot's length is not declared, or is not what arrived: {headers:?}"
    );
    assert!(
        !headers
            .iter()
            .any(|line| line.to_ascii_lowercase().starts_with("transfer-encoding")),
        "the snapshot was sent with a framing the protocol forbids: {headers:?}"
    );

    let (status, _, held) = send_bytes(&address, "GET", "/backup?as=log", None);
    assert_eq!(status, 200);
    let verified = tessari_backup::verify(&mut held.as_slice()).unwrap();
    assert!(verified.records > 0, "{verified:?}");
    assert!(!verified.truncated, "{verified:?}");
}

#[test]
fn the_backup_route_takes_one_query_and_names_the_mistake_of_any_other() {
    // Silently backing the whole store up when the caller asked for an increment
    // is a very expensive typo, so the route refuses rather than guessing.
    let (_node, address) = node();
    request(&address, "POST", "/script", READY);

    let (status, _, held) = send_bytes(&address, "GET", "/backup?from=1", None);
    assert_eq!(status, 200);
    assert!(tessari_backup::verify(&mut held.as_slice()).is_ok());

    let (status, _, held) = send_bytes(&address, "GET", "/backup?as=state", None);
    assert_eq!(status, 200);
    assert!(tessari_backup::verify_state(&mut held.as_slice()).is_ok());

    let (status, _, held) = send_bytes(&address, "GET", "/backup?as=script", None);
    assert_eq!(status, 200);
    assert!(
        String::from_utf8(held)
            .unwrap()
            .starts_with("-- TessariDB state script")
    );

    let (status, body) = request(&address, "GET", "/backup?since=1", "");
    assert_eq!(status, 400, "{body}");
    let (status, body) = request(&address, "GET", "/backup?as=logs", "");
    assert_eq!(status, 400, "{body}");
    let (status, body) = request(&address, "GET", "/backup?from=later", "");
    assert_eq!(status, 400, "{body}");
}

#[test]
fn the_backup_route_adds_no_permission_of_its_own() {
    // The identity rules are the statement's, so a viewer is refused here for
    // exactly the reason they are refused in the language — and an endpoint that
    // decided this for itself would be a second answer to a settled question.
    let (_node, address) = closed();
    let (status, _, body) = send(&address, "GET", "/backup", "", Some(GRACE));
    assert!(status >= 400, "a viewer downloaded the whole store: {body}");
    let (status, _, held) = send_bytes(&address, "GET", "/backup", Some(ROOT));
    assert_eq!(status, 200);
    assert!(tessari_backup::verify_state(&mut held.as_slice()).is_ok());
}

/// A JSON body, sent with the content type that says so.
fn json(address: &str, body: &str, credential: Option<&str>) -> (u16, String) {
    let mut stream = TcpStream::connect(address).unwrap();
    let authorization =
        credential.map_or_else(String::new, |value| format!("Authorization: {value}\r\n"));
    let head = format!(
        "POST /script HTTP/1.1\r\nHost: {address}\r\n{authorization}Content-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
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

#[test]
fn a_supplied_value_reaches_the_last_surface_that_could_not_take_one() {
    // The property SGA.T2 built was real and, over HTTP, unreachable: a caller
    // with a value to supply had no way to supply it, so they built a string —
    // and a property that protects nobody is not protection.
    let (_node, address) = node();
    request(&address, "POST", "/script", READY);
    request(
        &address,
        "POST",
        "/script",
        "USE NAMESPACE prod DATABASE orders; \
         CREATE users:1 = { name: 'ada' }; CREATE users:2 = { name: 'grace' };",
    );

    let (status, body) = json(
        &address,
        r#"{"script":"USE NAMESPACE prod DATABASE orders; SELECT * FROM users WHERE name = $who;","parameters":{"who":"'grace'"}}"#,
        None,
    );
    assert_eq!(status, 200, "{body}");
    assert!(body.contains("grace"), "{body}");
    assert!(!body.contains("ada"), "{body}");
}

#[test]
fn a_value_supplied_over_http_can_never_be_read_as_grammar() {
    // The same attempt the wire test makes, at the surface a caller most often
    // reaches for. Binding happens after parsing and before the first statement,
    // so this is a string that says something alarming.
    let (_node, address) = node();
    request(&address, "POST", "/script", READY);
    request(
        &address,
        "POST",
        "/script",
        "USE NAMESPACE prod DATABASE orders; CREATE users:1 = { name: 'ada' };",
    );

    let (status, body) = json(
        &address,
        r#"{"script":"USE NAMESPACE prod DATABASE orders; SELECT * FROM users WHERE name = $who;","parameters":{"who":"'; DROP TABLE users; --'"}}"#,
        None,
    );
    assert_eq!(status, 200, "{body}");

    // The table is still there, holding what it held.
    let (status, body) = request(
        &address,
        "POST",
        "/script",
        "USE NAMESPACE prod DATABASE orders; SELECT * FROM users;",
    );
    assert_eq!(status, 200, "{body}");
    assert!(body.contains("ada"), "the table did not survive: {body}");
}

#[test]
fn a_plain_body_is_still_the_script_it_always_was() {
    // Breaking `curl -d 'SELECT …'` to add a feature nobody using it asked for
    // would be a poor trade, so the shape is decided by the content type.
    let (_node, address) = node();
    let (status, body) = request(&address, "POST", "/script", READY);
    assert_eq!(status, 200, "{body}");
    assert_eq!(body.matches(r#""kind":"done""#).count(), 5, "{body}");
}

#[test]
fn a_body_that_is_not_the_envelope_says_which_part_was_wrong() {
    let (_node, address) = node();
    for (body, expected) in [
        (r#"{"parameters":{}}"#, "script"),
        (
            r#"{"script":"SELECT 1;","parameters":{"n":3}}"#,
            "TessariQL",
        ),
        (r#"not json at all"#, "expected"),
        // A value that is a statement rather than a value: refused before
        // anything runs, which is the CLI's rule at this surface.
        (
            r#"{"script":"SELECT 1;","parameters":{"n":"1; DROP TABLE users"}}"#,
            "not a value",
        ),
    ] {
        let (status, answered) = json(&address, body, None);
        assert_eq!(status, 400, "{body} answered {answered}");
        assert!(
            answered.contains(expected),
            "{body} answered {answered}, which does not mention {expected:?}"
        );
    }
}

#[test]
fn an_unbound_parameter_is_refused_in_the_sessions_own_words() {
    let (_node, address) = node();
    request(&address, "POST", "/script", READY);
    let (status, body) = json(
        &address,
        r#"{"script":"USE NAMESPACE prod DATABASE orders; SELECT * FROM users WHERE name = $who;"}"#,
        None,
    );
    assert!(status >= 400, "{body}");
    assert!(body.contains("who"), "{body}");
}

/// Send `POST /script` with `framing` in the head and `body` after it, and read
/// the status the node answers with.
///
/// The body is written from its own thread, so a node that answers before
/// reading all of it is heard rather than deadlocked against. The read gives up
/// after five seconds, which is what turns "the node is still reading" into a
/// failure instead of a hang.
fn status_for_body(address: &str, framing: &str, body: Vec<u8>) -> u16 {
    let mut stream = TcpStream::connect(address).unwrap();
    stream
        .set_read_timeout(Some(std::time::Duration::from_secs(5)))
        .unwrap();
    let head =
        format!("POST /script HTTP/1.1\r\nHost: {address}\r\n{framing}Connection: close\r\n\r\n");
    stream.write_all(head.as_bytes()).unwrap();
    let mut writer = stream.try_clone().unwrap();
    // A refusal may close the socket before the body is written, which is the
    // point, so a failed write here is expected rather than a test failure.
    let sending = std::thread::spawn(move || drop(writer.write_all(&body)));
    let mut status_line = String::new();
    BufReader::new(stream)
        .read_line(&mut status_line)
        .expect("an answer before the timeout");
    drop(sending.join());
    status_line
        .split_whitespace()
        .nth(1)
        .unwrap()
        .parse()
        .unwrap()
}

#[test]
fn a_body_declared_past_the_ceiling_is_refused_before_it_is_read() {
    // Nothing follows the head. A node that tried to read the declared body
    // would wait for bytes that never come, and the read above times out.
    let (_node, address) = node();
    let declared = HTTP_MAX_BODY_BYTES.saturating_add(1);
    let framing = format!("Content-Length: {declared}\r\n");
    assert_eq!(status_for_body(&address, &framing, Vec::new()), 413);
}

#[test]
fn a_body_that_declares_no_length_is_read_only_to_the_ceiling() {
    // Chunked, so no length is declared and only reading can find the size:
    // one mebibyte of spaces at a time, one chunk more than the ceiling holds.
    let (_node, address) = node();
    let chunk = vec![b' '; 1024 * 1024];
    let chunks = HTTP_MAX_BODY_BYTES.checked_div(chunk.len()).unwrap();
    let mut body = Vec::new();
    for _ in 0..=chunks {
        body.extend_from_slice(format!("{:x}\r\n", chunk.len()).as_bytes());
        body.extend_from_slice(&chunk);
        body.extend_from_slice(b"\r\n");
    }
    body.extend_from_slice(b"0\r\n\r\n");
    assert_eq!(
        status_for_body(&address, "Transfer-Encoding: chunked\r\n", body),
        413
    );
}

#[test]
fn a_scrape_says_where_a_follower_stands_and_how_far_behind_its_followers_are() {
    let db = Arc::new(Db::in_memory().unwrap());
    let node = Arc::new(Node::bind(Arc::clone(&db), "127.0.0.1:0").unwrap());
    let address = node.address();
    let serving = Arc::clone(&node);
    std::thread::spawn(move || crate::serve_until_the_test_ends(&serving));

    // Absent rather than zero on a node that follows nobody and that nobody
    // follows: a series that always reads zero teaches whoever watches it to
    // stop looking.
    let (_, _, quiet) = send(&address, "GET", "/metrics", "", None);
    assert!(!quiet.contains("tessari_replica_state"), "{quiet}");
    assert!(
        !quiet.contains("tessari_follower_behind_records"),
        "{quiet}"
    );

    let (_, _, _) = send(&address, "POST", "/script", "DEFINE NAMESPACE prod;", None);
    let follower = [7_u8; 16];
    db.store().follower_served(
        follower,
        tessari_types::Reach::Store,
        tessari_types::Sequence::new(0),
    );
    db.store().upstream_is(tessari_storage::Upstream::Copying);
    db.store().replica_copied(40);

    let (_, _, scrape) = send(&address, "GET", "/metrics", "", None);
    assert_eq!(
        counter(&scrape, "tessari_replica_state{state=\"catching up\"}"),
        1
    );
    assert_eq!(
        counter(&scrape, "tessari_replica_state{state=\"in sync\"}"),
        0
    );
    assert_eq!(counter(&scrape, "tessari_replica_copied_records"), 40);
    let behind = counter(
        &scrape,
        "tessari_follower_behind_records{node=\"07070707-0707-0707-0707-070707070707\"}",
    );
    assert!(
        behind > 0,
        "the follower was given nothing, so it is behind: {scrape}"
    );
}

#[test]
fn a_stranger_scraping_a_closed_store_learns_nothing_about_the_cluster() {
    // ADR-0108 D8: which nodes follow this one, how far behind each is, and
    // whether this node leads are the topology. On a closed store they are for
    // a caller who may ask `INFO FOR NODE`, and for nobody else.
    let db = Arc::new(Db::in_memory().unwrap());
    let node = Arc::new(Node::bind(Arc::clone(&db), "127.0.0.1:0").unwrap());
    let address = node.address();
    let serving = Arc::clone(&node);
    std::thread::spawn(move || crate::serve_until_the_test_ends(&serving));
    let (status, _, body) = send(
        &address,
        "POST",
        "/script",
        "DEFINE USER root ROLE owner PASSWORD 'root secret';",
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
    db.store().follower_served(
        [7_u8; 16],
        tessari_types::Reach::Store,
        tessari_types::Sequence::new(0),
    );
    db.store().upstream_is(tessari_storage::Upstream::Copying);
    db.store().across_sampled(0, 0);
    // ADR-0113 D4: a balanced table's shards as a pass measured them.
    db.store().shards_measured(vec![(
        tessari_types::TableId::new(9),
        tessari_storage::SampledTable {
            name: "prod.shop.orders".to_owned(),
            shards: vec![tessari_storage::SampledShard {
                shard: tessari_types::ShardId::new(1),
                records: 3,
                complete: true,
                writes_per_second: None,
            }],
            last_act: None,
        },
    )]);

    const CLUSTER: [&str; 14] = [
        "tessari_follower_behind_records",
        "tessari_replica_state",
        "tessari_replica_copied_records",
        "tessari_campaigns",
        "tessari_log_divergences",
        "tessari_not_held_here_total",
        "tessari_acknowledgement_waits_total",
        "tessari_acknowledgement_timeouts_total",
        "tessari_acknowledgement_wait_seconds_total",
        "tessari_transactions_across_leaders_total",
        "tessari_transactions_pending",
        "tessari_transactions_with_intents",
        "tessari_shard_records{table=\"prod.shop.orders\",shard=\"1\"} 3",
        "tessari_balancer_moves_total",
    ];
    for (who, credential) in [("a stranger", None), ("a viewer", Some(GRACE))] {
        let (status, _, scrape) = send(&address, "GET", "/metrics", "", credential);
        assert_eq!(status, 200, "{scrape}");
        // The control: the scrape is a real one, not an empty refusal.
        assert!(scrape.contains("tessari_committed_sequence"), "{scrape}");
        for series in CLUSTER {
            assert!(
                !scrape.contains(series),
                "{who} was shown {series}: {scrape}"
            );
        }
        assert!(!scrape.contains("07070707"), "{who} learnt a follower's id");
    }
    let (status, _, scrape) = send(&address, "GET", "/metrics", "", Some(ROOT));
    assert_eq!(status, 200, "{scrape}");
    for series in CLUSTER {
        assert!(
            scrape.contains(series),
            "the owner was not shown {series}: {scrape}"
        );
    }
}

#[test]
fn the_certificate_a_surface_presents_says_when_it_expires_and_only_to_an_operator() {
    // ADR-0108 D6: an expired certificate is a refused handshake, so its date
    // is a number to alert on — and which certificates a node holds is about
    // the cluster, so it sits behind the same gate as the topology (D8).
    const EXPIRES: i64 = 2_556_230_400; // 2051-01-02T00:00:00Z
    let mut params = rcgen::CertificateParams::new(vec!["localhost".to_owned()]).unwrap();
    params.not_after = rcgen::date_time_ymd(2051, 1, 2);
    let key = rcgen::KeyPair::generate().unwrap();
    let leaf =
        rustls::pki_types::CertificateDer::from(params.self_signed(&key).unwrap().der().to_vec());
    let mut census = tessari_serve::Census::since(std::time::Instant::now());
    census.presenting(
        "clients",
        tessari_serve::Presenting::new(move || Some(leaf.clone())),
    );
    let db = Arc::new(Db::in_memory().unwrap());
    let mut node = Node::bind(Arc::clone(&db), "127.0.0.1:0").unwrap();
    node.watching(Arc::new(census));
    let node = Arc::new(node);
    let address = node.address();
    let serving = Arc::clone(&node);
    std::thread::spawn(move || crate::serve_until_the_test_ends(&serving));
    let (status, _, body) = send(
        &address,
        "POST",
        "/script",
        "DEFINE USER root ROLE owner PASSWORD 'root secret';",
        None,
    );
    assert_eq!(status, 200, "{body}");

    let (status, _, scrape) = send(&address, "GET", "/metrics", "", None);
    assert_eq!(status, 200, "{scrape}");
    assert!(scrape.contains("tessari_committed_sequence"), "{scrape}");
    assert!(
        !scrape.contains("tessari_tls_certificate_expires_seconds"),
        "a stranger was shown the certificates: {scrape}"
    );

    let before = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let (status, _, scrape) = send(&address, "GET", "/metrics", "", Some(ROOT));
    assert_eq!(status, 200, "{scrape}");
    let left: i64 = scrape
        .lines()
        .find_map(|line| {
            line.strip_prefix("tessari_tls_certificate_expires_seconds{surface=\"clients\"} ")
        })
        .unwrap_or_else(|| panic!("no expiry line: {scrape}"))
        .parse()
        .unwrap();
    let expected = EXPIRES - i64::try_from(before).unwrap();
    assert!(
        (expected - 60..=expected).contains(&left),
        "{left} seconds left, expected about {expected}"
    );
}

#[test]
fn an_answer_given_before_the_body_was_needed_still_reaches_the_client() {
    // RFC 9112 §9.6: a server that closes with request bytes unread makes its
    // kernel send a reset, and a client still reading loses the answer it was
    // sent. A route that takes no body — here a path nothing serves — must
    // therefore still read the body it was sent before the connection closes.
    // Measured before the fix: one in two of these lost its answer (Q-935).
    let (_node, address) = node();
    let body = vec![b'x'; 1024 * 1024];
    for attempt in 0..20 {
        let mut stream = TcpStream::connect(&address).unwrap();
        stream
            .set_read_timeout(Some(std::time::Duration::from_secs(10)))
            .unwrap();
        write!(
            stream,
            "PUT /not/a/route/at/all HTTP/1.1\r\nHost: {address}\r\nContent-Length: {}\r\n\
             Connection: close\r\n\r\n",
            body.len()
        )
        .unwrap();
        // Half first, then a pause, so the node can answer before the rest is
        // sent — the order that loses the answer.
        let (first, rest) = body.split_at(body.len() / 2);
        stream.write_all(first).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(20));
        drop(stream.write_all(rest));
        let mut answer = Vec::new();
        match stream.read_to_end(&mut answer) {
            Ok(_) => assert!(
                answer.starts_with(b"HTTP/1.1 404"),
                "attempt {attempt}: {}",
                String::from_utf8_lossy(&answer)
            ),
            Err(failure) => panic!("attempt {attempt}: the answer was lost: {failure}"),
        }
    }
}
