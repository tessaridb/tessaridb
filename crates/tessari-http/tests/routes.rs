//! What a caller over the wire sees.
//!
//! The routes are exercised through a real socket rather than by calling the
//! handlers, because the thing being tested is that a *client* can talk to this
//! — and a handler called directly proves only that the function works.

#![allow(clippy::panic, clippy::unwrap_used, clippy::indexing_slicing)]

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::sync::Arc;

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
    std::thread::spawn(move || serving.serve());
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
    std::thread::spawn(move || serving.serve());

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
    std::thread::spawn(move || serving.serve());

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

    let (status, body) = request(&address, "GET", "/backup?since=1", "");
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
    assert!(tessari_backup::verify(&mut held.as_slice()).is_ok());
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
