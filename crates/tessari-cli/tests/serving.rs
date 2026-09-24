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

use std::collections::BTreeMap;
use std::net::TcpStream;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use tessari_wire::{Answer, Client, Served};

/// The binary this crate builds, which is the one an operator installs.
const TESSARIDB: &str = env!("CARGO_BIN_EXE_tessaridb");

/// The client surface of the node that also opens a peer door.
///
/// Its own port rather than a shared one, because every address in this file is
/// fixed: two tests reaching for the same number fail on whichever ran second,
/// for a reason that has nothing to do with what either asserts.
const WIRE_WITH_PEERS: &str = "127.0.0.1:47826";
/// That node's peer door.
const PEERS: &str = "127.0.0.1:47827";

/// A door this test owns and the node under test is expected to call.
///
/// Bound by the test itself rather than by a node, because what is being
/// observed is the DIAL: nothing has to answer it correctly for the arrival of
/// the connection to prove the cadence ran and reached the catalog.
const DIALLED: &str = "127.0.0.1:47828";

/// The dialling node's own peer door — a distinct address, because the wire
/// test above is holding 47827 and these two run in the same binary.
const PEERS_FOR_DIALLING: &str = "127.0.0.1:47830";

/// A second client surface, so the dialling test does not contend for 47826.
const WIRE_WITH_DIALLING: &str = "127.0.0.1:47829";

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
    let child = Command::new(TESSARIDB)
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
    let child = Command::new(TESSARIDB)
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

/// One GET, answered whole — or the reason it could not be asked.
///
/// A `Result` rather than a `String`, and that is the point of the helper: the
/// two ways a readiness probe fails are **it said the wrong thing** and
/// **nothing was listening**, and they mean opposite things to an operator. The
/// first is a node lying about itself; the second is a node that closed its
/// port at the moment its answer changed, which is the failure this whole stage
/// exists to prevent. Collapsed into one string, the second reads as the first.
fn probing(address: &str, path: &str) -> std::io::Result<String> {
    use std::io::{Read, Write};

    let mut socket = TcpStream::connect(address)?;
    let request = format!("GET {path} HTTP/1.1\r\nHost: {address}\r\nConnection: close\r\n\r\n");
    socket.write_all(request.as_bytes())?;
    socket.flush()?;
    let mut answered = String::new();
    socket.read_to_string(&mut answered)?;
    Ok(answered)
}

#[test]
fn a_node_says_it_is_not_ready_while_it_is_still_answering() {
    // Readiness and liveness are different questions and a supervisor acts on
    // them in opposite ways — one says stop sending traffic, the other says
    // restart. What is asserted here is not that the route replies, which a
    // constant `200` would satisfy: it is that the answer **changes** while the
    // node is still reachable to be asked.
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("store");
    let wire = "127.0.0.1:47851";
    let http = "127.0.0.1:47852";

    let node = serving_both(&path, wire, http);

    let willing = probing(http, "/ready").expect("the readiness route never answered");
    assert!(
        willing.starts_with("HTTP/1.1 200"),
        "a node that had not been asked to stop did not call itself ready: {willing}"
    );

    // All three of the node's status routes, read from one running process,
    // which is what F9 asks for — and the third is here for a claim of its own:
    // the metrics gauge and the readiness route report the **same** state. Two
    // places answering one question is two places for it to be answered
    // differently, so they are checked against each other rather than each
    // against its own idea of the truth.
    let alive = probing(http, "/health").expect("the health route never answered");
    assert!(alive.starts_with("HTTP/1.1 200"), "{alive}");
    let scraped = probing(http, "/metrics").expect("the metrics route never answered");
    assert!(
        scraped.contains(r#"tessari_ready{surface="http"} 1"#),
        "the scrape disagreed with the readiness route about a node that is \
         ready: {scraped}"
    );

    let signalled = Command::new("kill")
        .args(["-TERM", &node.0.id().to_string()])
        .status()
        .unwrap();
    assert!(signalled.success(), "the signal was not delivered");

    // Bounded by less than the lame-duck window on purpose. Inside it the port
    // must still be open, so a connection failure here is a real failure and
    // not the shutdown having simply moved on.
    let mut leaving = None;
    let began = Instant::now();
    while began.elapsed() < Duration::from_secs(4) {
        match probing(http, "/ready") {
            Ok(answered) if answered.starts_with("HTTP/1.1 503") => {
                leaving = Some(answered);
                break;
            }
            Ok(_) => std::thread::yield_now(),
            Err(why) => panic!(
                "the node stopped accepting connections before it said it was \
                 not ready, so nothing routing traffic here could ever read the \
                 answer: {why}"
            ),
        }
    }
    let leaving = leaving.expect("the node went on calling itself ready after being told to stop");
    assert!(leaving.contains(r#""status":"leaving""#), "{leaving}");

    // And the gauge moved with it. A constant satisfies neither half of this:
    // it was `1` above and must be `0` now.
    let scraped = probing(http, "/metrics").expect("the metrics route stopped answering");
    assert!(
        scraped.contains(r#"tessari_ready{surface="http"} 0"#),
        "the readiness route said it was leaving and the scrape still reports \
         it ready, so a dashboard and a load balancer would disagree: {scraped}"
    );

    // Liveness is unmoved. A supervisor that restarted this node now would be
    // restarting one that is shutting down on purpose.
    let alive = probing(http, "/health").expect("the health route stopped answering");
    assert!(alive.starts_with("HTTP/1.1 200"), "{alive}");

    drop(node);
}

#[test]
fn a_scrape_describes_the_whole_process_and_not_one_listener() {
    // The census is what makes this possible and it is the claim: the metrics
    // route is served by HTTP but must report the **wire** protocol's counters
    // too, which that surface has never seen. A scrape naming only its own
    // surface is what this fails on.
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("store");
    let wire = "127.0.0.1:47861";
    let http = "127.0.0.1:47862";

    let node = serving_both(&path, wire, http);

    // Traffic on the wire surface, so its counters are not zero for the trivial
    // reason. A scrape that only ever sees zeros cannot show they are wrong.
    {
        let mut client = Client::connect(wire).unwrap();
        client
            .run("DEFINE NAMESPACE prod; USE NAMESPACE prod;", None)
            .unwrap();
        // And one refusal, which is the other half of the definition.
        drop(client.run("SELECT * FROM nothing_defined_here;", None));
    }

    let scraped = probing(http, "/metrics").expect("the metrics route never answered");
    assert!(scraped.starts_with("HTTP/1.1 200"), "{scraped}");
    assert!(
        scraped.contains(r#"tessari_answers_total{surface="wire"}"#)
            && scraped.contains(r#"tessari_answers_total{surface="http"}"#),
        "the scrape did not name both surfaces, so it describes a listener \
         rather than the process: {scraped}"
    );
    assert!(
        scraped.contains("tessari_uptime_seconds"),
        "a process that knows when it started reported no uptime: {scraped}"
    );

    // The wire surface answered, and said no at least once.
    let answered = scraped
        .lines()
        .find_map(|line| line.strip_prefix(r#"tessari_answers_total{surface="wire"} "#))
        .expect("no wire answer count in the scrape");
    assert!(
        answered.trim().parse::<u64>().unwrap() > 0,
        "the wire surface answered requests and the scrape reports none: {scraped}"
    );

    drop(node);
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
                 DEFINE COLLECTION users; CREATE users:1 = { who: 'ada' };",
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
                 DEFINE COLLECTION users; CREATE users:1 = { who: 'ada' };",
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
                 DEFINE COLLECTION users;",
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

    let second = Command::new(TESSARIDB)
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
    let mut refused = Command::new(TESSARIDB)
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

/// A certificate authority minted for one test, and a leaf it issues.
///
/// In memory and then written to the temporary directory, because the binary
/// takes paths — but never a fixture committed to the repository, which would be
/// key material with an expiry date nobody chose.
/// A well-formed seed for a node that never dials one.
///
/// `<node-id>@<host:port>` since ADR-0067 — a bare address is refused at start,
/// because the handshake derives the peer's TLS name from its id and so an
/// address with no id attached is not a dial this transport can express.
const A_SEED: &str = "1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a1a@one.example:9080";

struct Minted {
    authority: rcgen::Certificate,
    key: rcgen::KeyPair,
}

impl Minted {
    fn new() -> Self {
        let mut params = rcgen::CertificateParams::new(Vec::new()).unwrap();
        params.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
        let key = rcgen::KeyPair::generate().unwrap();
        let authority = params.self_signed(&key).unwrap();
        Self { authority, key }
    }

    /// A credential naming `node` on the peer link, as PEM.
    fn issue(&self, node: [u8; 16]) -> (String, String) {
        let name = tessari_wire::names(node, tessari_wire::Purpose::Peer);
        let params = rcgen::CertificateParams::new(vec![name]).unwrap();
        let key = rcgen::KeyPair::generate().unwrap();
        let leaf = params.signed_by(&key, &self.authority, &self.key).unwrap();
        (leaf.pem(), key.serialize_pem())
    }
}

/// Write a credential for `node`, plus the authority, into `into`.
fn credentials(
    minted: &Minted,
    node: [u8; 16],
    into: &std::path::Path,
) -> (String, String, String) {
    let (leaf, key) = minted.issue(node);
    let at = |name: &str| into.join(name).to_string_lossy().into_owned();
    std::fs::write(at("leaf.pem"), leaf).unwrap();
    std::fs::write(at("key.pem"), key).unwrap();
    std::fs::write(at("ca.pem"), minted.authority.pem()).unwrap();
    (at("leaf.pem"), at("key.pem"), at("ca.pem"))
}

#[test]
fn a_node_told_about_a_cluster_opens_its_peer_door_and_still_serves_clients() {
    // The wave's own claim, against the shipped binary: the five cluster flags
    // reach `Peers::bind`, the door is listening at the address the operator
    // named, and the client surface is unaffected by its presence. The wire
    // crate proves what the door DOES once a peer arrives; nothing but this
    // proves the flags ever reach it.
    let directory = tempfile::tempdir().unwrap();
    let store = directory.path().join("store");
    let minted = Minted::new();
    // Opened and closed again to learn the id the store generated for itself.
    // The binary then starts on the same directory and is therefore the same
    // node: an identity is generated once and is stable across restarts, which
    // is exactly the property this relies on.
    let db = tessaridb::Db::open(&store).unwrap();
    let identity = db.store().node_identity().unwrap();
    let node = identity.id;
    let build = identity.version;
    // One write, so the log this node will greet with is not empty. A fresh
    // store's tail is legitimately zero — nothing is written on open, and the
    // first-user bootstrap writes only when both environment variables are set —
    // so without this the greeting's tail could not tell what the store holds
    // apart from a constant somebody typed.
    db.session().run("DEFINE NAMESPACE probe;").unwrap();
    drop(db);
    let (leaf, key, authority) = credentials(&minted, node, directory.path());

    let child = Command::new(TESSARIDB)
        .arg(&store)
        .args(["--serve", WIRE_WITH_PEERS])
        .args(["--cluster-credential", &leaf])
        .args(["--cluster-key", &key])
        .args(["--cluster-authority", &authority])
        .args(["--cluster-address", PEERS])
        .args(["--seed", A_SEED])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    // Killed on the way out however this test ends, including a panic.
    let _running = Running(child);

    assert!(
        listening(PEERS, Duration::from_secs(20)),
        "the peer door never accepted a connection"
    );
    assert!(
        listening(WIRE_WITH_PEERS, Duration::from_secs(20)),
        "the client surface stopped serving because a peer door was opened"
    );

    // And it greets as ITSELF. The TLS handshake below only completes against a
    // server whose certificate carries `<node id>.peer.tessari`, so reaching a
    // greeting at all proves the binary loaded the credential the flags named;
    // the greeting then proves it answered under the identity its own store
    // holds rather than under anything it was told.
    let caller = [7u8; 16];
    let (their_leaf, their_key) = minted.issue(caller);
    // Read through the same parser the binary uses, so the test cannot pass on
    // a credential the node itself would have refused to load.
    let ours = tessari_wire::Joining::parse(
        their_leaf.as_bytes(),
        std::path::Path::new("leaf.pem"),
        their_key.as_bytes(),
        std::path::Path::new("key.pem"),
        minted.authority.pem().as_bytes(),
        std::path::Path::new("ca.pem"),
        PEERS.to_owned(),
        vec![A_SEED.to_owned()],
    )
    .expect("a credential this authority issued");
    let (heard, answered) = tessari_wire::call(
        PEERS,
        ours.mine,
        &ours.authority,
        node,
        &tessari_wire::Hello {
            node: caller,
            build,
            epoch: tessari_types::Epoch::ZERO,
            roles: tessari_storage::Roles::NONE,
            tail: tessari_types::Sequence::new(0),
            tail_leadership: tessari_types::Epoch::ZERO,
            current_as_of: None,
            policy: None,
            line: None,
        },
        tessari_wire::Ask::Nothing,
    )
    .expect("a peer holding a credential this cluster issued is answered");

    assert_eq!(heard.node, node, "the node greets under its own identity");
    // Read from the store at greeting time rather than fixed: this node wrote
    // its first user during startup, so a tail of zero would mean the greeting
    // carries a constant somebody typed instead of what the log actually holds.
    assert!(
        heard.tail.get() > 0,
        "the greeting carries the log this node really holds, not a placeholder"
    );
    assert_eq!(
        answered,
        tessari_wire::Answered::Nothing,
        "nothing was asked, so nothing was answered"
    );
}

#[test]
fn a_peer_address_that_cannot_be_taken_is_a_failure_to_start_and_not_a_warning() {
    // Port 1 is not takeable by an unprivileged process, which is the cheapest
    // unbindable address there is. What is asserted is the ORDER: a node that
    // cannot take its peer address must not reach the point of answering
    // clients, because a node silently outside its cluster looks exactly like
    // one that started correctly.
    let directory = tempfile::tempdir().unwrap();
    let store = directory.path().join("store");
    let minted = Minted::new();
    let (leaf, key, authority) = credentials(&minted, [3u8; 16], directory.path());

    let mut refused = Command::new(TESSARIDB)
        .arg(&store)
        .args(["--serve", "127.0.0.1:0"])
        .args(["--cluster-credential", &leaf])
        .args(["--cluster-key", &key])
        .args(["--cluster-authority", &authority])
        .args(["--cluster-address", "127.0.0.1:1"])
        .args(["--seed", A_SEED])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    drop(refused.stdin.take());
    let done = refused.wait_with_output().unwrap();
    assert!(
        !done.status.success(),
        "a door that cannot open is a failure"
    );
    let said = String::from_utf8_lossy(&done.stderr);
    // The address it could not take, named — so the operator is not left
    // comparing five flags against a bare permission error.
    assert!(said.contains("127.0.0.1:1"), "{said}");
    assert!(
        !said.contains("wire protocol on"),
        "the client surface must never have been announced: {said}"
    );
}

#[test]
fn a_clustered_node_with_no_seed_and_no_peer_refuses_to_start() {
    // The half of `Told::from_parts`'s old five-part rule that was doing real
    // work, restated where it can be true. Four cluster parts and no seed is a
    // legal configuration — it is the founding node, and every node whose
    // catalog already names somebody — so the flag parser cannot decide this
    // one: *can this node reach anybody* is answered by the seeds OR by the
    // store, and it sees only the first (Q-577).
    //
    // A node with neither has no route into the cluster it was configured for.
    // It would come up, refresh nothing, and go on serving whatever it last
    // collected, with nothing anywhere in an error state — so it fails to
    // start, and the refusal names both halves rather than one.
    let directory = tempfile::tempdir().unwrap();
    let store = directory.path().join("store");
    let minted = Minted::new();
    let (leaf, key, authority) = credentials(&minted, [7u8; 16], directory.path());

    let mut refused = Command::new(TESSARIDB)
        .arg(&store)
        .args(["--serve", "127.0.0.1:0"])
        .args(["--cluster-credential", &leaf])
        .args(["--cluster-key", &key])
        .args(["--cluster-authority", &authority])
        .args(["--cluster-address", "127.0.0.1:0"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    drop(refused.stdin.take());
    let done = refused.wait_with_output().unwrap();
    assert!(
        !done.status.success(),
        "a node with no route into its cluster is a failure to start"
    );
    let said = String::from_utf8_lossy(&done.stderr);
    assert!(
        said.contains("no seed") && said.contains("no peer"),
        "the refusal must name both halves, because fixing either one settles \
         it and the operator has to be told which they have: {said}"
    );
    assert!(
        !said.contains("wire protocol on"),
        "the client surface must never have been announced: {said}"
    );
}

/// The self-declaring cluster, and the joiner that restarts with no seed.
///
/// A clean band: 47891-47894 belong to the join above.
const NAMED: [(&str, &str); 2] = [
    ("127.0.0.1:47895", "127.0.0.1:47896"),
    ("127.0.0.1:47897", "127.0.0.1:47898"),
];

/// G025 S5.1 — a cluster declares a membership row for itself.
///
/// # What the self-row buys, and why nothing else can buy it
///
/// The row a cluster writes to admit a newcomer describes the NEWCOMER, so a
/// joiner's first collection leaves it holding exactly one membership row: its
/// own. That row names nobody to follow — `upstream` and `greet_round` both
/// skip it — and the only route the joiner has to the leader is the address on
/// its command line. Lose the flag and restart, and the node is alone with a
/// catalog that is about itself (Q-579).
///
/// Every consensus system's membership list contains every member *including
/// the one reading it*. So the node holding the original declares a row for
/// itself, replicated like any other record, and what the joiner collects then
/// names somebody. The seed becomes what §6.3 always said it was — an address
/// for first contact, spent as soon as it works.
///
/// # It is a declaration and not something the node writes
///
/// `DEFINE REPLICA` is an operator's word everywhere else in this engine, for
/// the reason `replica.rs` states: a self-maintained membership row needs a
/// heartbeat, and a heartbeat is failure detection. Nothing new is built here.
/// The row was always expressible — `ROLES` and `NODE` both already take what
/// it needs — and what was missing is that the seed could not be dropped, so
/// the row it makes redundant could never be observed doing its job.
///
/// # The assertion is a write made AFTER the restart
///
/// Not the presence of the row, which proves only that a collection once
/// happened, and not the records collected before the restart, which the seed
/// could account for. A record written on the leader while the joiner is
/// running without a seed can have arrived by one route only.
#[test]
#[ignore = "two minutes of real cadences against three process starts; run it with \
            cargo test -p tessari-cli --test serving a_joiner_restarted -- --ignored"]
fn a_joiner_restarted_without_its_seed_still_finds_the_leader() {
    let directory = tempfile::tempdir().unwrap();
    let minted = Minted::new();

    let mut stores = Vec::new();
    let mut ids = Vec::new();
    let mut papers = Vec::new();
    for (index, _) in NAMED.iter().enumerate() {
        let home = directory.path().join(format!("n{index}"));
        std::fs::create_dir_all(&home).unwrap();
        let store = home.join("store");
        let db = tessaridb::Db::open(&store).unwrap();
        let id = db.store().node_identity().unwrap().id;
        drop(db);
        papers.push(credentials(&minted, id, &home));
        ids.push(id);
        stores.push(store);
    }

    // The cluster side, and it is the join test's two acts plus ONE statement:
    // the leader declares a row for itself. `ROLES serving, writable` is what
    // `Roles::ALONE` already is, so the row agrees with what this node is
    // rather than asking it to become something.
    {
        let db = tessaridb::Db::open(&stores[0]).unwrap();
        let joiner = tessari_types::RecordId::Uuid(ids[1]).to_string();
        let leader = tessari_types::RecordId::Uuid(ids[0]).to_string();
        db.session()
            .run(&format!(
                "DEFINE REPLICA joiner AT '{}' NODE '{joiner}' ROLES serving \
                 REPLICATES STORE; \
                 DEFINE REPLICA origin AT '{}' NODE '{leader}' ROLES serving, writable; \
                 {WRITTEN}",
                NAMED[1].1, NAMED[0].1
            ))
            .expect("a leader that knows who is joining it, and who it is itself");
        drop(db);
    }
    {
        let db = tessaridb::Db::open(&stores[1]).unwrap();
        db.session()
            .run("DEFINE NODE ROLES serving;")
            .expect("a node that knows it is not the cluster");
        drop(db);
    }

    let start = |index: usize, seed: bool| {
        let (leaf, key, authority) = &papers[index];
        let mut command = Command::new(TESSARIDB);
        command
            .arg(&stores[index])
            .args(["--serve", NAMED[index].0])
            .args(["--cluster-credential", leaf])
            .args(["--cluster-key", key])
            .args(["--cluster-authority", authority])
            .args(["--cluster-address", NAMED[index].1]);
        if seed {
            let other = usize::from(index == 0);
            command.args([
                "--seed",
                &format!(
                    "{}@{}",
                    tessari_types::RecordId::Uuid(ids[other]),
                    NAMED[other].1
                ),
            ]);
        }
        Running(
            command
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .unwrap(),
        )
    };

    // The leader keeps its seed for the whole test. It never reads it — its
    // catalog names a peer from the first statement — and leaving it in place
    // keeps the joiner as the only variable.
    let leader = start(0, true);
    {
        let joiner = start(1, true);
        for (client, peer) in NAMED {
            assert!(listening(client, Duration::from_secs(30)), "{client}");
            assert!(listening(peer, Duration::from_secs(30)), "{peer}");
        }
        let began = Instant::now();
        let mut joined = false;
        let mut refusals = Vec::new();
        while began.elapsed() < Duration::from_secs(90) && !joined {
            match counted(NAMED[1].0) {
                Ok(1) => joined = true,
                Ok(other) => refusals.push(format!("{other} record(s)")),
                Err(why) => refusals.push(why),
            }
            std::thread::sleep(POLL);
        }
        assert!(
            joined,
            "the joiner never reached the leader's records with its seed, so \
             the restart below would prove nothing; the last thing it said was \
             {:?}",
            refusals.last()
        );
        drop(joiner);
    }

    // Restarted with the four cluster parts and NO seed. Everything it now
    // knows about where the cluster is, it collected.
    let joiner = start(1, false);
    assert!(
        listening(NAMED[1].0, Duration::from_secs(30)),
        "the joiner refused to start without a seed, and its catalog names the \
         leader — the refusal is for a node that can reach NOBODY"
    );

    {
        let mut client = Client::connect(NAMED[0].0).expect("the leader's client door");
        client
            .run(
                "USE NAMESPACE prod; USE DATABASE orders; CREATE item:2 = { n: 2 };",
                None,
            )
            .expect("a write on the one node that takes writes");
    }
    let since = Instant::now();
    let mut arrived = None;
    let mut silence = Vec::new();
    while since.elapsed() < Duration::from_secs(90) && arrived.is_none() {
        match counted(NAMED[1].0) {
            Ok(2) => arrived = Some(since.elapsed()),
            Ok(other) => silence.push(format!("{other} record(s)")),
            Err(why) => silence.push(why),
        }
        std::thread::sleep(POLL);
    }
    let arrived = arrived.unwrap_or_else(|| {
        panic!(
            "the joiner restarted without a seed and never collected again: a \
             record written after the restart never arrived, and the last thing \
             it said was {:?}",
            silence.last()
        )
    });
    assert!(
        arrived < Duration::from_secs(90),
        "the restarted joiner took {arrived:?} to receive a write made after it \
         came back"
    );
    drop(joiner);
    drop(leader);
}

/// The row declared without `NODE`, and the joiner that binds it by arriving.
///
/// A clean band: 47895-47898 belong to the self-declaring cluster above.
const UNBOUND: [(&str, &str); 2] = [
    ("127.0.0.1:47899", "127.0.0.1:47900"),
    ("127.0.0.1:47901", "127.0.0.1:47902"),
];

/// The replica row named `joiner`, as the node at `address` reports it.
///
/// Read through `INFO FOR NODE` rather than off the disk, because a binding an
/// operator cannot read back is a binding they cannot check, and this is the
/// surface they would check it on. The whole row and not one field: the
/// criterion is as much about what did NOT change as about what did.
fn joiner_row(address: &str) -> Result<BTreeMap<String, tessari_types::Value>, String> {
    let mut client = Client::connect(address).map_err(|why| why.to_string())?;
    let answers = client
        .run("INFO FOR NODE;", None)
        .map_err(|why| why.to_string())?;
    let Some(Answer::Value {
        value: tessari_types::Value::Object(report),
        ..
    }) = answers.last()
    else {
        return Err(format!("not a report: {answers:?}"));
    };
    let Some(tessari_types::Value::Object(cluster)) = report.get("cluster") else {
        return Err(format!("no cluster group: {report:?}"));
    };
    let Some(tessari_types::Value::Array(peers)) = cluster.get("peers") else {
        return Err(format!("no peers: {cluster:?}"));
    };
    peers
        .iter()
        .find_map(|peer| match peer {
            tessari_types::Value::Object(fields)
                if fields.get("name") == Some(&tessari_types::Value::from("joiner")) =>
            {
                Some(fields.clone())
            }
            _ => None,
        })
        .ok_or_else(|| format!("no row named joiner among {peers:?}"))
}

/// What that row says its node is — `Ok(None)` while nobody has bound it.
fn bound_node(address: &str) -> Result<Option<String>, String> {
    match joiner_row(address)?.get("node") {
        Some(tessari_types::Value::Uuid(bytes)) => {
            Ok(Some(tessari_types::RecordId::Uuid(*bytes).to_string()))
        }
        Some(tessari_types::Value::Null) | None => Ok(None),
        other => Err(format!("node is {other:?}")),
    }
}

/// G025 S5.2 — a replica row's node id is bound by the first inbound greeting.
///
/// # Why inbound is the only route, and therefore not a choice
///
/// A row whose `node` is `None` is declared but **undiallable**:
/// `Directory::greet_round` skips it, because opening a session derives the
/// peer's transport name from its identifier and an unbound row has none to
/// derive from. So this node can never bind the row by reaching out. The single
/// event that can ever bind it is that peer arriving here and proving who it is
/// — which is what the criterion means by *the first inbound greeting*, and it
/// is a statement about the design rather than a preference between two.
///
/// # The greeting supplies the id and nothing else
///
/// W281 made a row's `roles` decide whether this node is fenced, so a row bound
/// by a greeting is a row that can move the write gate. That is Q-553's own
/// recorded objection to binding by greeting — *a self-binding row is a role
/// somebody else's first packet gets to assign* — and the answer is that only
/// `node` is written. The endpoint, the roles and the reach stay exactly as the
/// operator declared them, which the assertion below checks rather than assumes.
///
/// # The assertion is the transition, not the end state
///
/// The row is read once **before** the joiner starts and asserted unbound. A
/// test that only checked the end state would pass against a fixture that had
/// been bound all along, which is the failure mode this whole criterion is about.
#[test]
#[ignore = "a minute of real cadences against two process starts; run it with \
            cargo test -p tessari-cli --test serving a_row_nobody_bound -- --ignored"]
fn a_row_nobody_bound_is_bound_by_the_peer_that_arrives() {
    let directory = tempfile::tempdir().unwrap();
    let minted = Minted::new();

    let mut stores = Vec::new();
    let mut ids = Vec::new();
    let mut papers = Vec::new();
    for (index, _) in UNBOUND.iter().enumerate() {
        let home = directory.path().join(format!("n{index}"));
        std::fs::create_dir_all(&home).unwrap();
        let store = home.join("store");
        let db = tessaridb::Db::open(&store).unwrap();
        let id = db.store().node_identity().unwrap().id;
        drop(db);
        papers.push(credentials(&minted, id, &home));
        ids.push(id);
        stores.push(store);
    }

    // The one difference from the self-declaring cluster above: the `joiner`
    // row carries no `NODE`. The operator wrote down where the peer will be and
    // what it is for, and left the identity to be proved rather than typed.
    {
        let db = tessaridb::Db::open(&stores[0]).unwrap();
        let leader = tessari_types::RecordId::Uuid(ids[0]).to_string();
        db.session()
            .run(&format!(
                "DEFINE REPLICA joiner AT '{}' ROLES serving REPLICATES STORE; \
                 DEFINE REPLICA origin AT '{}' NODE '{leader}' ROLES serving, writable; \
                 {WRITTEN}",
                UNBOUND[1].1, UNBOUND[0].1
            ))
            .expect("a leader that declared a peer it has not met");
        drop(db);
    }
    {
        let db = tessaridb::Db::open(&stores[1]).unwrap();
        db.session()
            .run("DEFINE NODE ROLES serving;")
            .expect("a node that knows it is not the cluster");
        drop(db);
    }

    let start = |index: usize| {
        let (leaf, key, authority) = &papers[index];
        let mut command = Command::new(TESSARIDB);
        command
            .arg(&stores[index])
            .args(["--serve", UNBOUND[index].0])
            .args(["--cluster-credential", leaf])
            .args(["--cluster-key", key])
            .args(["--cluster-authority", authority])
            .args(["--cluster-address", UNBOUND[index].1]);
        // Both nodes are seeded, and the leader's seed is not decoration. Its
        // catalog names an unbound row and its own, so `names_a_peer` answers
        // no — it names nobody it could follow — and W281's startup refusal
        // would stop it dead. It never reads the seed, because nothing it needs
        // is anywhere else; carrying it keeps the binding as the only variable,
        // which is the shape the S5.1 scenario above settled on for the same
        // reason. Q-612 records that the origin of a cluster should not need one.
        let other = usize::from(index == 0);
        command.args([
            "--seed",
            &format!(
                "{}@{}",
                tessari_types::RecordId::Uuid(ids[other]),
                UNBOUND[other].1
            ),
        ]);
        Running(
            command
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .unwrap(),
        )
    };

    let leader = start(0);
    assert!(
        listening(UNBOUND[0].0, Duration::from_secs(30)),
        "the leader never opened its client door"
    );
    assert_eq!(
        bound_node(UNBOUND[0].0),
        Ok(None),
        "the row must start unbound, or what follows proves nothing"
    );

    let joiner = start(1);
    assert!(
        listening(UNBOUND[1].0, Duration::from_secs(30)),
        "the joiner never opened its client door"
    );

    let expected = tessari_types::RecordId::Uuid(ids[1]).to_string();
    let began = Instant::now();
    let mut bound = false;
    let mut answers = Vec::new();
    while began.elapsed() < Duration::from_secs(60) && !bound {
        match bound_node(UNBOUND[0].0) {
            Ok(Some(said)) if said == expected => bound = true,
            Ok(other) => answers.push(format!("{other:?}")),
            Err(why) => answers.push(why),
        }
        std::thread::sleep(POLL);
    }
    assert!(
        bound,
        "the joiner greeted the leader and the leader never bound the row it \
         declared for it; the last thing the leader said was {:?}",
        answers.last()
    );

    // The other half of the rule, and the reason it is asserted here rather
    // than trusted: the greeting carried an epoch, a role set and a log tail,
    // and exactly none of them is allowed to reach this row. Asserted on the
    // joiner's OWN row — the leader's `origin` row beside it really is
    // `writable`, so a check over the whole report would pass on the wrong row.
    let row = joiner_row(UNBOUND[0].0).expect("the joiner's row after binding");
    assert_eq!(
        row.get("endpoint"),
        Some(&tessari_types::Value::from(UNBOUND[1].1)),
        "the endpoint the operator wrote must survive the binding: {row:?}"
    );
    assert_eq!(
        row.get("roles"),
        Some(&tessari_types::Value::Array(vec![
            tessari_types::Value::from("serving")
        ])),
        "the joiner was declared `serving` and greeted carrying `serving, \
         writable`; the greeting must not have moved its roles: {row:?}"
    );

    drop(joiner);
    drop(leader);
}

#[test]
fn a_node_dials_the_peer_its_catalog_declares() {
    // W239's own claim against the shipped binary. `tessari-wire` proves what a
    // greeting round DOES to a directory; nothing but this proves the binary
    // ever runs one. The observable is deliberately the weakest thing that can
    // only happen if the whole chain worked: a TCP connection arriving at an
    // address that appears nowhere but in this node's own replica catalog.
    //
    // The listener answers nothing, so the dial fails its TLS handshake. That is
    // the correct outcome to assert on: `greet_round` records a failure and
    // moves on, and a wave that needed the far end to be a real node would be
    // testing two nodes rather than this node's cadence.
    let waiting = std::net::TcpListener::bind(DIALLED).expect("the test's own door");
    waiting
        .set_nonblocking(true)
        .expect("polled rather than blocked, so the assertion can time out");

    let directory = tempfile::tempdir().unwrap();
    let store = directory.path().join("store");
    let minted = Minted::new();
    let db = tessaridb::Db::open(&store).unwrap();
    let node = db.store().node_identity().unwrap().id;
    // A peer that is emphatically NOT this node: `greet_round` skips its own row
    // on purpose, so declaring this node's own id here would make the test pass
    // for the wrong reason — or rather, fail for the right one.
    let peer = tessari_types::RecordId::Uuid([9u8; 16]).to_string();
    db.session()
        .run(&format!(
            "DEFINE REPLICA watcher AT '{DIALLED}' NODE '{peer}' ROLES serving;"
        ))
        .expect("a declared peer with an id is the only diallable kind");
    drop(db);
    let (leaf, key, authority) = credentials(&minted, node, directory.path());

    let child = Command::new(TESSARIDB)
        .arg(&store)
        .args(["--serve", WIRE_WITH_DIALLING])
        .args(["--cluster-credential", &leaf])
        .args(["--cluster-key", &key])
        .args(["--cluster-authority", &authority])
        .args(["--cluster-address", PEERS_FOR_DIALLING])
        .args(["--seed", A_SEED])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let _running = Running(child);

    // The first pass runs BEFORE the first wait — `every` checks the flag, runs
    // the pass, and only then sleeps — so this arrives at startup rather than an
    // awareness interval later. A test that had to wait ten seconds for it would
    // be asserting the sleep, not the dial.
    let began = Instant::now();
    let mut dialled = false;
    while began.elapsed() < Duration::from_secs(20) {
        match waiting.accept() {
            Ok(_) => {
                dialled = true;
                break;
            }
            Err(_) => std::thread::yield_now(),
        }
    }
    assert!(
        dialled,
        "the node never dialled the peer its own catalog declares"
    );
}

/// A three-node cluster's addresses — a client surface and a peer door each.
///
/// A named type rather than the array spelled out at four signatures, and a
/// parameter rather than a constant read inside the bring-up, because two
/// `#[ignore]`d cluster tests in this file are run by one
/// `cargo test … -- --ignored` and the harness runs them on separate threads.
/// Sharing a band would make each one fail intermittently on the other's
/// listener, which reads exactly like the cluster defect neither test is about.
type Band = [(&'static str, &'static str); 3];

/// The three-node cluster's addresses — a client surface and a peer door each.
///
/// A band of its own rather than the next free pair, because this test holds six
/// addresses at once: a collision would fail it for a reason that has nothing to
/// do with a failover, which is the most expensive kind of failure to read.
const CLUSTER: [(&str, &str); 3] = [
    ("127.0.0.1:47881", "127.0.0.1:47882"),
    ("127.0.0.1:47883", "127.0.0.1:47884"),
    ("127.0.0.1:47885", "127.0.0.1:47886"),
];

/// The schema and the first record, written by whichever node is leading.
///
/// It is also the election probe. Under ADR-0064 a node that takes part in
/// deciding and holds no leadership refuses every write with
/// `NoLeadershipYet` — so the node that accepts this script **is** the one a
/// majority granted the epoch to, and asking costs nothing beyond the write the
/// test needed anyway.
const SCHEMA: &str = "DEFINE NAMESPACE prod REPLICATION FACTOR 2; USE NAMESPACE prod; \
                      DEFINE DATABASE orders; USE DATABASE orders; \
                      DEFINE COLLECTION item; CREATE item:1 = { n: 1 };";

/// How often anything in this test asks a node a question.
///
/// A busy loop is not free here: every pass opens a TCP connection, and a spin
/// exhausts the machine's ephemeral port range in seconds — at which point the
/// nodes' own peer dials start failing and the cluster appears to be broken.
const POLL: Duration = Duration::from_millis(100);

/// The read the client keeps making, on every node, throughout.
const READ: &str = "USE NAMESPACE prod; USE DATABASE orders; SELECT * FROM item;";

/// What one read saw, and how far into the scenario it saw it.
struct Saw {
    at: Duration,
    outcome: Result<usize, String>,
}

/// Read `READ` from `address` once, counting the records or naming the refusal.
fn counted(address: &str) -> Result<usize, String> {
    let mut client = Client::connect(address).map_err(|why| why.to_string())?;
    // The last answer, because `run` answers once per statement and the read is
    // the last of three — the two that select a tenancy answer `Done`.
    let answers = client.run(READ, None).map_err(|why| why.to_string())?;
    match answers.last() {
        Some(Answer::Records { records, .. }) => Ok(records.len()),
        other => Err(format!("not a read: {other:?}")),
    }
}

/// How many leadership rounds `address` has stood in.
///
/// From `INFO FOR NODE`, which reads the same `health()` the `/metrics` scrape
/// does — so the quiet-cluster assertion below and an operator's dashboard
/// cannot disagree about the number.
fn campaigns(address: &str) -> Result<i64, String> {
    let mut client = Client::connect(address).map_err(|why| why.to_string())?;
    let answers = client
        .run("INFO FOR NODE;", None)
        .map_err(|why| why.to_string())?;
    let Some(Answer::Value {
        value: tessari_types::Value::Object(report),
        ..
    }) = answers.last()
    else {
        return Err(format!("not a report: {answers:?}"));
    };
    let Some(tessari_types::Value::Object(cluster)) = report.get("cluster") else {
        return Err(format!("no cluster group: {report:?}"));
    };
    match cluster.get("campaigns") {
        Some(tessari_types::Value::Number(tessari_types::Number::Integer(stood))) => Ok(*stood),
        other => Err(format!("campaigns is {other:?}")),
    }
}

/// The node after `index`, wrapping round — the peer each node names as its seed.
///
/// Spelled without arithmetic on purpose. These three lines moved out of a
/// `#[test]` body, where clippy exempts index arithmetic, into a plain function
/// where it does not; and the exemption is the only thing that had been
/// carrying them. Written this way it also survives a band gaining a fourth
/// entry, which a hardcoded wrap would not.
fn the_next_node(band: &Band, index: usize) -> usize {
    index
        .checked_add(1)
        .filter(|next| *next < band.len())
        .unwrap_or(0)
}

/// A cluster of three, brought up the one way that works.
///
/// Factored out of the S7.1 test it was written inside, because three further
/// criteria need exactly this arrangement: the `ANSWERED BY LEADER` redirect
/// across processes, the superseded-epoch refusal, and a failover policy
/// reaching a second node. A second copy would be a second place the
/// declaration ORDER is decided, and that order is the part nobody rediscovers
/// correctly — the comments inside say why each step is where it is, and they
/// were each bought by a deadlock.
struct Three {
    /// Held so the stores outlive the processes reading them. Dropping this
    /// removes the directory, so it is a field rather than a discarded local.
    _directory: tempfile::TempDir,
    /// One slot per node, in band order. A slot is emptied to kill that
    /// node, which is why it is an `Option` rather than a plain handle.
    running: Vec<Option<Running>>,
    /// Where each node writes its own stderr, in band order.
    ///
    /// Kept because the directory holding them is removed the moment this
    /// struct drops, so a reader who goes looking after a failure finds
    /// nothing at all. A panic message is the only place these lines can still
    /// be read — the same reason `the_node_a_majority_granted` carries its
    /// refusals into its own.
    logs: Vec<std::path::PathBuf>,
}

/// The tail of each node's own log, for a panic that would otherwise send its
/// reader to three processes that no longer exist.
///
/// Every failure the collection and awareness cadences can have is reported
/// through `log::warn!`, which this binary writes to standard error and nowhere
/// else — a peer that did not answer, a subscription nobody granted, a log that
/// no longer reaches back far enough. A harness that discards that stream can
/// say a cluster replicated nothing and can never say why.
///
/// `TESSARIDB_LOG` is inherited from whoever ran the test, so `debug` is a
/// command-line decision rather than a property of the fixture.
///
/// # What is dropped, and why it is safe to drop
///
/// Every `connection N accepted` / `connection N closed` pair, and nothing
/// else. Those lines are THIS TEST'S OWN polling: `counted` opens a client
/// connection every hundred milliseconds for ninety seconds, so a follower's
/// log reaches nineteen hundred lines of which eighteen hundred and ninety are
/// the harness watching itself. A window that keeps them shows the reader the
/// test's footprint and none of the cluster's. The count of what was dropped is
/// printed beside the window, so the filter can be challenged from the output
/// it produces rather than only from this comment.
fn what_the_nodes_said(band: &[(&str, &str)], logs: &[std::path::PathBuf]) -> String {
    /// With this test's own polling removed a ninety-second three-node run
    /// leaves each node about a hundred and twenty lines, so this is a ceiling
    /// against a node that is genuinely looping rather than a window that
    /// trims a healthy run. A cluster that replicates nothing says so in the
    /// FIRST cadence, and a tail short enough to lose that first cadence is a
    /// diagnostic that reports only the symptom.
    const TAIL: usize = 200;
    let mut out = String::new();
    for (index, path) in logs.iter().enumerate() {
        let read = std::fs::read_to_string(path)
            .unwrap_or_else(|why| format!("this node's log could not be read: {why}"));
        let all = read.lines().count();
        let lines: Vec<&str> = read
            .lines()
            .filter(|line| !line.contains("tessari_wire::node connection "))
            .collect();
        let from = lines.len().saturating_sub(TAIL);
        out.push_str(&format!(
            "\n--- node {index} ({}), last {} of {} line(s); {} of this test's \
             own connection lines dropped ---\n",
            band[index].0,
            lines.len().saturating_sub(from),
            lines.len(),
            all.saturating_sub(lines.len())
        ));
        for line in &lines[from..] {
            out.push_str(line);
            out.push('\n');
        }
    }
    out
}

/// Bring three nodes up, declared as one cluster, and wait for every door.
///
/// It does NOT wait for an election — that is [`the_node_a_majority_granted`],
/// because a caller that only needs three live nodes should not pay ninety
/// seconds for a leader it will not use.
fn a_cluster_of_three(band: &Band) -> Three {
    let directory = tempfile::tempdir().unwrap();
    let minted = Minted::new();

    // Each node's id is generated by its own store on first open, so it has to
    // be read before anything can declare it. Its own directory each, because
    // `credentials` writes three fixed filenames.
    let mut stores = Vec::new();
    let mut ids = Vec::new();
    let mut papers = Vec::new();
    for index in 0..band.len() {
        let home = directory.path().join(format!("n{index}"));
        std::fs::create_dir_all(&home).unwrap();
        let store = home.join("store");
        let db = tessaridb::Db::open(&store).unwrap();
        let id = db.store().node_identity().unwrap().id;
        drop(db);
        papers.push(credentials(&minted, id, &home));
        ids.push(id);
        stores.push(store);
    }

    // Each node declares the WHOLE membership, itself included, in one order.
    //
    // It declared only the other two until W382, on the reasoning that a self
    // row would make the membership `peers.len() + 1` = four and demand three
    // grants. That reasoning was right about the arithmetic and wrong about
    // where to fix it: a membership row is an ordinary catalog record, so it
    // REPLICATES (`catalog/replica.rs`), and a follower that applies a leader's
    // store log receives a row naming itself no matter what it declared. The
    // arithmetic is now corrected where it is decided, in `voters`, which skips
    // this node exactly as `greet_round` and `upstream` already did.
    //
    // What declaring the full set buys is the ID. A replica row is written
    // under a locally allocated number, so three nodes that each declare a
    // DIFFERENT pair allocate the same numbers to different peers — and the
    // first replication then overwrites each follower's row for the leader with
    // the leader's row for somebody else. Measured in W382: a follower
    // collected once, lost the row naming its upstream, and never collected
    // again, while every node reported two healthy peers. Declaring the same
    // three rows in the same order makes the replication idempotent instead.
    //
    // `writable` on every row and not just on this node's own: the row is what
    // `Store::reconcile_roles` reads back at open as what this node is SUPPOSED
    // to be, so a row that omits it drains the node the next time it opens —
    // and ADR-0063/ADR-0064 make every-coordinator-also-writable the
    // configuration a cluster needs in order to fail over at all.
    //
    // `REPLICATES STORE` rather than the namespace the criterion names, and the
    // reason is a property of the engine rather than a convenience: a namespace
    // subscription resolves to a namespace **id** when the row is written, so
    // the name has to exist on the granting node first. ADR-0063 means any of
    // the three may win, so no node can be pinned as the granter before the
    // election. There is no narrowing step anywhere below, and the sentence
    // that used to promise one here was describing work nobody wrote: the
    // subscription this fixture actually grants is the whole store, on every
    // node, and the criterion's replication is observed over that.
    for store in &stores {
        let db = tessaridb::Db::open(store).unwrap();
        // The peers FIRST and the role LAST, and the order is load-bearing.
        // `DEFINE NODE ROLES` takes effect immediately and locally, so a node
        // that adopts `coordinating` holds no lease from that instant and
        // refuses every local write with `NoLeadershipYet` (ADR-0064) — while
        // `DEFINE REPLICA` is a catalog write and therefore a log record. The
        // obvious order deadlocks: the statement that makes a node a member is
        // the statement that stops it accepting the ones that describe the
        // cluster. Everything a coordinating node needs written locally is
        // written before it becomes one; afterwards, configuration is the
        // leader's to write and replicate, which is what it should be.
        // ONE transaction, and that is not tidiness. `DEFINE REPLICA` is a
        // catalog write and therefore a log record, and the moment the FIRST
        // one commits this node names a peer in committed membership — which is
        // the condition ADR-0069 put the write gate on. Statement by statement,
        // the second declaration is refused `NoLeadershipYet` by the first.
        // A cluster is declared atomically or not at all.
        let mut script = String::from("BEGIN;");
        for other in 0..band.len() {
            let named = tessari_types::RecordId::Uuid(ids[other]).to_string();
            script.push_str(&format!(
                " DEFINE REPLICA n{other} AT '{}' NODE '{named}' \
                  ROLES serving, writable, coordinating REPLICATES STORE;",
                band[other].1
            ));
        }
        script.push_str(" DEFINE NODE ROLES serving, writable, coordinating; COMMIT;");
        db.session()
            .run(&script)
            .expect("a cluster of three, declared");
        drop(db);
    }

    let mut running: Vec<Option<Running>> = Vec::new();
    let mut logs: Vec<std::path::PathBuf> = Vec::new();
    for index in 0..band.len() {
        let (leaf, key, authority) = &papers[index];
        // Standard error is where this binary reports, so it is kept rather
        // than discarded. Its own file per node: three streams into one
        // descriptor interleave, and a log line carries a timestamp and a
        // target but never says which node wrote it.
        let log = directory.path().join(format!("n{index}")).join("node.log");
        let writing = std::fs::File::create(&log).unwrap();
        logs.push(log);
        let child = Command::new(TESSARIDB)
            .arg(&stores[index])
            .args(["--serve", band[index].0])
            .args(["--cluster-credential", leaf])
            .args(["--cluster-key", key])
            .args(["--cluster-authority", authority])
            .args(["--cluster-address", band[index].1])
            // A seed names the node as well as the address (ADR-0067): the
            // handshake derives the peer's TLS name from its id, so a bare
            // address is not a dial this transport can express. These three
            // declare each other in their catalogs, so the seed is never read
            // — it is here because a running node takes the flag.
            .args([
                "--seed",
                &format!(
                    "{}@{}",
                    tessari_types::RecordId::Uuid(ids[the_next_node(band, index)]),
                    band[the_next_node(band, index)].1
                ),
            ])
            .stdout(Stdio::null())
            .stderr(Stdio::from(writing))
            .spawn()
            .unwrap();
        running.push(Some(Running(child)));
    }
    for (client, peer) in band {
        assert!(listening(client, Duration::from_secs(30)), "{client}");
        assert!(listening(peer, Duration::from_secs(30)), "{peer}");
    }
    Three {
        _directory: directory,
        running,
        logs,
    }
}

/// The index of the node a majority granted the epoch to.
///
/// Asked by trying to use it rather than by reading anything: under ADR-0064 a
/// node that takes part in deciding and holds no leadership refuses every write
/// with `NoLeadershipYet`, so the node that accepts the script **is** the one
/// that won, and asking costs nothing beyond the write a caller needed anyway.
fn the_node_a_majority_granted(band: &Band) -> usize {
    // Wait for the first epoch to be granted, by trying to use it. A write
    // before any round concludes is refused on every node — that is ADR-0064
    // working, not a defect, and it is why this polls rather than writing once.
    let began = Instant::now();
    let mut elected = None;
    let mut refusals = [String::new(), String::new(), String::new()];
    while began.elapsed() < Duration::from_secs(90) && elected.is_none() {
        for (index, (surface, _)) in band.iter().enumerate() {
            if let Ok(mut client) = Client::connect(surface) {
                match client.run(SCHEMA, None) {
                    Ok(_) => {
                        elected = Some(index);
                        break;
                    }
                    // Kept so the failure below can name the refusal. A test
                    // that reports only *nobody wrote* sends the next reader to
                    // the logs of three processes to learn something the client
                    // was told every second.
                    Err(why) => refusals[index] = why.to_string(),
                }
            }
        }
        // Paced, and this is not politeness. A spin here opens three TCP
        // connections per iteration and the first draft managed nine thousand a
        // second — enough to exhaust this machine's ephemeral ports, after
        // which the NODES' own peer dials began failing with `Can't assign
        // requested address`. The harness was breaking the thing it was
        // measuring, and the symptom looked exactly like a cluster defect.
        std::thread::sleep(POLL);
    }
    elected.unwrap_or_else(|| {
        panic!(
            "no node accepted a write in ninety seconds. A cluster of three \
             that elects nobody has no writer anywhere, which is what ADR-0064 \
             made possible and what an election is supposed to resolve. The \
             last refusal from each node: {refusals:?}"
        )
    })
}

#[test]
#[ignore = "forty seconds of real cadences — a leader has to be elected, a \
            record replicated, a node killed and a successor elected, and none \
            of those can be hurried. It is criterion S7.1's own validation and \
            is run explicitly, following the S6.2 validation in \
            tessari-wire/tests/pushing.rs: cargo test -p tessari-cli --test \
            serving three_nodes -- --ignored"]
fn three_nodes_elect_lose_their_leader_and_go_on_answering() {
    // G024 S7.1, the last criterion, against three operating-system processes.
    //
    // Everything the wire and storage crates prove about this is proved inside
    // one process, against stores a test assembled. What only this can show is
    // that the three decisions of this session compose: ADR-0063 lets a second
    // node stand, ADR-0064 makes winning confer the right to write, ADR-0065
    // lets a follower find whoever won. Each was found by the next one failing,
    // and none of them has ever been exercised against a cluster of three.
    let mut cluster = a_cluster_of_three(&CLUSTER);
    // Cloned before `running` is borrowed, because a panic below wants the
    // whole struct while that borrow is still live.
    let logs = cluster.logs.clone();
    let running = &mut cluster.running;
    let leader = the_node_a_majority_granted(&CLUSTER);
    // The record reaches a node that never wrote it. Until this holds there is
    // no replication to lose, so a failover asserted before it would be a
    // failover of nothing.
    //
    // WHICH follower is not something this test may choose. The schema declares
    // `REPLICATION FACTOR 2`, so the copy lands on the leader and **one** other
    // node — and which one is decided by an election this test deliberately
    // does not pin, since ADR-0063 lets any of the three win. Asking a
    // particular follower was therefore a coin flip: it passed when the copy
    // happened to go to the first node that was not the leader, and reported
    // *the follower never received the leader's record* when it went to the
    // other, which reads like a replication failure and is not one.
    //
    // So the question asked here is the one the factor actually promises — that
    // the record reached SOME node that did not write it — and the node that
    // has it becomes the one the reader below uses, instead of a second
    // independent guess at the same thing.
    let began = Instant::now();
    let mut holder = None;
    while began.elapsed() < Duration::from_secs(90) && holder.is_none() {
        holder = (0..CLUSTER.len())
            .filter(|index| *index != leader)
            .find(|index| counted(CLUSTER[*index].0) == Ok(1));
        if holder.is_none() {
            std::thread::sleep(POLL);
        }
    }
    let follower = holder.unwrap_or_else(|| {
        panic!(
            "no node but the leader received the record in ninety seconds, so \
             nothing below would be measuring a cluster. Counts: {:?}{}",
            (0..CLUSTER.len())
                .map(|index| counted(CLUSTER[index].0))
                .collect::<Vec<_>>(),
            what_the_nodes_said(&CLUSTER, &logs)
        )
    });

    // The criterion's SECOND half, and it has to be here rather than in its own
    // scenario: a fast failover and a quiet cluster are satisfiable by opposite
    // wrong changes — delete the standing gate and failover is quick and the
    // cluster campaigns forever; leave everything alone and it is quiet and
    // slow. Only a fixture that shows both at once distinguishes the right
    // change from either.
    //
    // A leader is holding a lease and renewing it against these voters, so a
    // follower that can hear it must not be standing. The count is read twice
    // across a window longer than a whole lease: a follower that campaigns does
    // so every second, so a flat number over ten seconds is not a sampling
    // accident.
    let quiet: Vec<i64> = (0..CLUSTER.len())
        .filter(|index| *index != leader)
        .map(|index| campaigns(CLUSTER[index].0).expect("a follower reports its rounds"))
        .collect();
    std::thread::sleep(Duration::from_secs(12));
    for (offset, index) in (0..CLUSTER.len())
        .filter(|index| *index != leader)
        .enumerate()
    {
        let now = campaigns(CLUSTER[index].0).expect("a follower reports its rounds");
        assert_eq!(
            now,
            quiet[offset],
            "follower {index} stood {} time(s) in twelve seconds while a leader \
             held its lease and renewed against it. A cluster that campaigns \
             against a live leader spends the one resource an election needs — \
             its voters' willingness to grant anything — and each round is a \
             self-vote that refuses the real leader's next renewal for a whole \
             lease (ADR-0066)",
            now - quiet[offset]
        );
    }

    // The criterion's own words: the client's behaviour ACROSS the window. A
    // reader that stops at the kill and resumes afterwards proves the cluster
    // recovered and says nothing about what anyone saw meanwhile, so this one
    // runs on its own thread for the whole of it and every outcome is kept.
    let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let reading = {
        let stop = std::sync::Arc::clone(&stop);
        let address = CLUSTER[follower].0;
        let from = Instant::now();
        std::thread::spawn(move || {
            let mut seen = Vec::new();
            while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                seen.push(Saw {
                    at: from.elapsed(),
                    outcome: counted(address),
                });
                // Often enough to describe the window, rarely enough not to be
                // the reason the window looks the way it does.
                std::thread::sleep(POLL);
            }
            seen
        })
    };

    // Uncatchably, as the kill test above does: a leader that leaves politely
    // demotes itself, and a demotion is not the loss this criterion names.
    running[leader] = None;

    // An automatic failover: a node the operator never touched begins accepting
    // writes. The catalog is not edited, no statement is run against a survivor
    // to promote it, and nothing outside the cluster chooses.
    let began = Instant::now();
    let mut successor = None;
    while began.elapsed() < Duration::from_secs(120) && successor.is_none() {
        for (index, (surface, _)) in CLUSTER.iter().enumerate() {
            if index == leader {
                continue;
            }
            if let Ok(mut client) = Client::connect(surface) {
                if client
                    .run(
                        "USE NAMESPACE prod; USE DATABASE orders; \
                         CREATE item:2 = { n: 2 };",
                        None,
                    )
                    .is_ok()
                {
                    successor = Some(index);
                    break;
                }
            }
        }
        std::thread::sleep(POLL);
    }
    // Read before the `expect`, so a failure reports how long it waited rather
    // than only that it gave up.
    let took = began.elapsed();
    let successor = successor.expect(
        "the cluster lost its leader and never got another: two of three nodes \
         were up, which is a majority, so a round could have been carried",
    );
    assert_ne!(successor, leader, "the killed node cannot be the successor");

    // G025 S2.2's timed half. *Faster than the awareness round* is a claim about
    // two numbers the constants already fix: a voter hears a live leader's
    // renewal about every two round times, while the greeting directory
    // refreshes every `AWARENESS_SECONDS` and a reading is itself up to that old
    // again — `STALENESS_FLOOR_SECONDS`, which is the worst case a node relying
    // on the directory alone would have to wait through.
    //
    // The bound is therefore the directory's worst case and not a tight fit to
    // whatever a run happens to show: a threshold fitted to an observation
    // asserts that the machine is as fast as it was the day it was measured.
    // The lease term is spent in both arms and is common-mode; what this bound
    // separates is the signal.
    eprintln!("failover took {took:?}");
    assert!(
        took < Duration::from_secs(tessari_constants::STALENESS_FLOOR_SECONDS),
        "the cluster took {took:?} to accept a write after losing its leader, \
         which is at or past the {}s a node with nothing but the greeting \
         directory would have to wait — so nothing here shows the peer link \
         detecting the loss any faster than the awareness round does",
        tessari_constants::STALENESS_FLOOR_SECONDS
    );

    stop.store(true, std::sync::atomic::Ordering::Relaxed);
    let seen = reading.join().expect("the reading thread panicked");

    // The assertion is on the SEQUENCE, not on a before-and-after pair. A record
    // this client was once given is never afterwards missing from an answer, and
    // no answer holds a record no leader ever acknowledged. Refusals during the
    // window are the design working: they are counted and reported, not failed.
    let mut high = 0usize;
    let mut refused = 0usize;
    for saw in &seen {
        match &saw.outcome {
            Ok(count) => {
                assert!(
                    *count >= high,
                    "a read {:?} into the scenario returned {count} records \
                     after an earlier one returned {high}: the client was given \
                     an answer contradicting one it already had",
                    saw.at
                );
                assert!(
                    *count <= 2,
                    "a read returned {count} records and only two were ever \
                     written, so a node answered with something no leader \
                     acknowledged"
                );
                high = *count;
            }
            Err(_) => refused += 1,
        }
    }
    assert!(
        high >= 1,
        "the client never once saw the record across the whole window, so \
         nothing here describes a cluster that kept answering"
    );
    assert!(
        seen.len() > refused,
        "every one of the {} reads across the window was refused",
        seen.len()
    );
}

/// The second cluster's addresses — 47874-47879, and a band of its own.
///
/// Both cluster tests in this file are `#[ignore]`d, and one
/// `cargo test … -- --ignored` runs them on two threads at once. Sharing
/// 47881-47886 would make each fail intermittently on the other's listener,
/// which reads exactly like the cluster defect neither test is about.
const AXES: Band = [
    ("127.0.0.1:47874", "127.0.0.1:47875"),
    ("127.0.0.1:47876", "127.0.0.1:47877"),
    ("127.0.0.1:47878", "127.0.0.1:47879"),
];

/// The policy cluster's addresses — 47863-47868, and a band of its own.
///
/// A third band for the same reason the second one exists: one
/// `cargo test … -- --ignored` runs every `#[ignore]`d test in this file on
/// separate threads, so two clusters sharing a band fail intermittently on each
/// other's listener — which reads exactly like the cluster defect neither test
/// is about. Derived from the ports this crate already spells, not guessed:
/// 47861-47862 are taken and 47863-47870 are not.
const PERIODS: Band = [
    ("127.0.0.1:47863", "127.0.0.1:47864"),
    ("127.0.0.1:47865", "127.0.0.1:47866"),
    ("127.0.0.1:47867", "127.0.0.1:47868"),
];

/// One password for every account this test declares.
///
/// One value and not three, because what these accounts are FOR is being looked
/// up on a node that never declared them — three secrets would be three chances
/// to mistype one into an assertion that then proves nothing by succeeding.
const SECRET: &str = "a long enough password";

/// Ask `address` as `who`, and give back the answers or the refusal as text.
///
/// Text rather than the error type, for [`counted`]'s reason: what a caller
/// here does with a refusal is put it in a panic message, and every one of
/// these questions is asked in a loop that must not stop on the first no.
fn asked(address: &str, script: &str, who: Option<(&str, &str)>) -> Result<Vec<Answer>, String> {
    let mut client = Client::connect(address).map_err(|why| why.to_string())?;
    client.run(script, who).map_err(|why| why.to_string())
}

/// Poll `question` until it is true, or give up after `within`.
///
/// Paced by `POLL` for the reason the election poll above is: a spin opens a
/// TCP connection per pass and exhausts this machine's ephemeral ports, after
/// which the NODES' peer dials start failing and the harness has broken the
/// thing it is measuring.
fn until(within: Duration, mut question: impl FnMut() -> bool) -> bool {
    let began = Instant::now();
    while began.elapsed() < within {
        if question() {
            return true;
        }
        std::thread::sleep(POLL);
    }
    false
}

/// Whether `script`, asked of `address` as `who`, answers with `token` in it.
///
/// A rendered answer searched for a token, and that is weaker than reading the
/// field — so the tokens below are ones this test CHOSE (`carol`, `item`) and
/// asserted absent before the act that should introduce them. A search that is
/// controlled on both sides cannot pass by finding the word somewhere else.
fn says(address: &str, who: Option<(&str, &str)>, script: &str, token: &str) -> bool {
    asked(address, script, who).is_ok_and(|answers| format!("{answers:?}").contains(token))
}

/// G029 S1.2, S3.1 and S3.3 against three operating-system processes.
///
/// Each of those three criteria is PARTIAL for one reason and it is the same
/// reason: the machinery is proven inside a single process, and the criterion's
/// own stated validation is a LIVE multi-node run. The harness that makes one
/// possible went green in W382; this is what it was made green for.
///
/// # The order is decided by one measured fact
///
/// **A store with no users is open, and declaring the first user closes it**
/// (`tessari-session/src/identity.rs`). Every other helper in this file connects
/// anonymously, so the moment this test declares a user it changes the access
/// posture of every node that applies the record. Everything anonymous
/// therefore happens BEFORE that statement, and everything after it carries
/// credentials.
///
/// That same fact is then used as an instrument rather than worked around: the
/// signal that the user record REACHED the follower is that the follower stops
/// answering an anonymous read. It is read without attempting a single sign-in,
/// which matters — the sign-in path throttles a name that keeps missing,
/// doubling from 250 ms towards thirty seconds, so polling for an account by
/// trying to use it makes the wait grow faster than replication closes it.
///
/// # Why a sign-in is still made, once
///
/// Because a name arriving and a CREDENTIAL arriving are different claims. The
/// single authenticated call proves the stored hash is the one the leader
/// wrote and that it verifies against a password this test never sent to this
/// node.
#[test]
#[ignore = "two minutes of real cadences against three spawned processes: an \
            election, then five replication waits at the collection cadence. \
            It is the live validation G029 S1.2, S3.1 and S3.3 each name, and \
            is run explicitly: cargo test -p tessari-cli --test serving \
            identity_and_a_leader_only_read -- --ignored"]
fn identity_and_a_leader_only_read_cross_three_processes() {
    let cluster = a_cluster_of_three(&AXES);
    // Cloned before anything can panic holding a borrow of the struct.
    let logs = cluster.logs.clone();
    let leader = the_node_a_majority_granted(&AXES);
    let follower = the_next_node(&AXES, leader);
    let surface = AXES[follower].0;
    let deciding = AXES[leader].0;
    // Long enough to cover several collection cadences, short enough that a
    // cluster which is not replicating at all fails this test rather than the
    // harness timeout, where the reason would be lost.
    let patience = Duration::from_secs(90);

    assert!(
        until(patience, || counted(surface) == Ok(1)),
        "the record never reached the follower, so nothing below would be \
         measuring a cluster.{}",
        what_the_nodes_said(&AXES, &logs)
    );

    // G029 S1.2 — the redirect, read off the wire.
    //
    // `run_routed` and deliberately not `run`: `run` folds a redirect into
    // `Error::Redirected`, and a test that matched on that would be inferring
    // the frame from an error type. The criterion says the frame kind reaches
    // the wire, so what is asserted is the variant the frame parser produced.
    let mut client = Client::connect(surface).expect("a follower answers its door");
    let served = client
        .run_routed(
            "USE NAMESPACE prod; USE DATABASE orders; \
             SELECT * FROM item ANSWERED BY LEADER;",
            None,
            &tessari_ql::Parameters::new(),
        )
        .expect("a follower says where a leader-only read belongs");
    let Served::Elsewhere(elsewhere) = served else {
        panic!(
            "a follower answered a read only the leader may answer: {served:?}.{}",
            what_the_nodes_said(&AXES, &logs)
        );
    };
    assert_eq!(
        elsewhere.endpoint, AXES[leader].1,
        "the redirect names an address that is not the leader's declared one"
    );
    assert!(
        elsewhere.epoch.get() > 0,
        "the redirect carries no leadership: {elsewhere:?}"
    );
    drop(client);

    // G029 S3.1 — a user declared on the leader, arriving on a node that never
    // saw the statement. This is the last anonymous act.
    asked(
        deciding,
        &format!("DEFINE USER ada ROLE owner PASSWORD '{SECRET}';"),
        None,
    )
    .expect("a leader declares the first user");

    assert!(
        until(patience, || matches!(
            counted(surface),
            Err(ref why) if why.contains("signed-in user")
        )),
        "the follower still answers an anonymous read, so the user record never \
         arrived — and the follower is not merely slow, it is open.{}",
        what_the_nodes_said(&AXES, &logs)
    );

    let owner = Some(("ada", SECRET));
    asked(surface, "INFO FOR USERS;", owner)
        .expect("a follower knows the leader's owner, and her password verifies there");

    // G029 S3.3 — a grant is a second record in a second table, so it is
    // asserted separately from the user it is about rather than inferred from
    // it. Both tokens are checked absent first: a rendered answer searched for
    // a word proves nothing unless the word was demonstrably not already there.
    assert!(
        !says(surface, owner, "INFO FOR USERS;", "carol"),
        "the follower already knows a user this test has not declared"
    );
    asked(
        deciding,
        &format!(
            "USE NAMESPACE prod; USE DATABASE orders; \
             DEFINE USER carol ON prod.orders ROLE viewer PASSWORD '{SECRET}';"
        ),
        owner,
    )
    .expect("a leader declares a second user");
    assert!(
        until(patience, || says(
            surface,
            owner,
            "INFO FOR USERS;",
            "carol"
        )),
        "a user declared on the leader never reached the follower.{}",
        what_the_nodes_said(&AXES, &logs)
    );

    let carols = "USE NAMESPACE prod; USE DATABASE orders; INFO FOR USER carol;";
    assert!(
        !says(surface, owner, carols, "item"),
        "the follower already carries a grant this test has not made"
    );
    // TWO grants, and the second is not decoration. A user's grants, if they
    // have any, are the whole story, so taking the LAST one away would widen
    // them back to everything their role allows — and the engine refuses that
    // rather than doing it quietly. Measured on this test's first run: *"that
    // is carol's last grant, and taking it away would widen them to every table
    // their role allows"*. A second grant makes the revocation below a
    // narrowing, which is the act this criterion is about.
    asked(
        deciding,
        "USE NAMESPACE prod; USE DATABASE orders; DEFINE COLLECTION note; \
         GRANT read ON item TO carol; GRANT read ON note TO carol;",
        owner,
    )
    .expect("a leader grants two tables to a user");
    assert!(
        until(patience, || says(surface, owner, carols, "item")),
        "a grant made on the leader never reached the follower.{}",
        what_the_nodes_said(&AXES, &logs)
    );

    // The removing direction, for both records, and it is the half an add-only
    // assertion cannot see: a replica that applies writes and loses deletes
    // keeps an authority the operator has taken away, and every assertion above
    // would still pass on one.
    asked(
        deciding,
        "USE NAMESPACE prod; USE DATABASE orders; REVOKE read ON item FROM carol;",
        owner,
    )
    .expect("a leader revokes");
    assert!(
        until(patience, || !says(surface, owner, carols, "item")),
        "a revocation made on the leader never reached the follower, which \
         still reports the grant.{}",
        what_the_nodes_said(&AXES, &logs)
    );
    // The other grant is still there, so what arrived was a revocation of ONE
    // table and not the user going missing — which is the only other way the
    // assertion above could have turned true.
    assert!(
        says(surface, owner, carols, "note"),
        "the revocation took more than it was asked for, or carol is gone \
         entirely.{}",
        what_the_nodes_said(&AXES, &logs)
    );

    asked(deciding, "DROP USER carol;", owner).expect("a leader drops a user");
    assert!(
        until(patience, || !says(
            surface,
            owner,
            "INFO FOR USERS;",
            "carol"
        )),
        "a user dropped on the leader is still present on the follower.{}",
        what_the_nodes_said(&AXES, &logs)
    );
}

/// The leader of a one-node cluster, and the node that joins it by seed alone.
///
/// A clean band: 47881-47886 belong to the three-node failover above.
const JOIN: [(&str, &str); 2] = [
    ("127.0.0.1:47891", "127.0.0.1:47892"),
    ("127.0.0.1:47893", "127.0.0.1:47894"),
];

/// What the joiner must end up holding, written only on the node it joins.
const WRITTEN: &str = "DEFINE NAMESPACE prod REPLICATION NONE; USE NAMESPACE prod; \
                       DEFINE DATABASE orders; USE DATABASE orders; \
                       DEFINE COLLECTION item; CREATE item:1 = { n: 1 };";

/// G024's title, which no criterion asked for — Q-570.
///
/// Every one of the goal's fifteen criteria passes and a cluster is still
/// assembled by configuring each node before any of them starts. This is the
/// other shape: one node that is already a cluster, and a second that is told
/// exactly one thing — an address to reach it through — and writes nothing of
/// its own.
///
/// # What is asserted, and why it is the records rather than the catalog
///
/// The joiner is checked for the LEADER'S DATA, not for its membership rows.
/// That is the stronger assertion and it is also the cheaper one: the
/// membership arrives because `DEFINE REPLICA` is a catalog write and therefore
/// already a log record, so a joiner holding the leader's records has
/// necessarily collected the rows that came before them. Asserting the rows
/// directly would test the same thing one step earlier and would pass on a node
/// that had collected the catalog and then stopped.
///
/// # The two acts that add a node, and which side each is on
///
/// On the cluster: one `DEFINE REPLICA` naming the newcomer and granting it the
/// log. On the newcomer: `DEFINE NODE ROLES serving` — a `META` write, local and
/// deliberately **not** a log record (ADR-0018), so it creates none of the
/// divergence W256 recorded when three nodes each wrote their own membership at
/// the same sequences under `Epoch::ZERO`.
#[test]
#[ignore = "a minute of real cadences against two spawned processes; run it with \
            cargo test -p tessari-cli --test serving a_node_joins -- --ignored"]
fn a_node_joins_a_cluster_it_was_only_given_an_address_for() {
    let directory = tempfile::tempdir().unwrap();
    let minted = Minted::new();

    let mut stores = Vec::new();
    let mut ids = Vec::new();
    let mut papers = Vec::new();
    for (index, _) in JOIN.iter().enumerate() {
        let home = directory.path().join(format!("j{index}"));
        std::fs::create_dir_all(&home).unwrap();
        let store = home.join("store");
        let db = tessaridb::Db::open(&store).unwrap();
        let id = db.store().node_identity().unwrap().id;
        drop(db);
        papers.push(credentials(&minted, id, &home));
        ids.push(id);
        stores.push(store);
    }

    // The cluster side of adding a node: the leader is told who the newcomer is
    // and that it may take the log. `REPLICATES STORE` because the namespace
    // does not exist yet on either node, and a namespace subscription resolves
    // to a namespace id when the row is written.
    //
    // The leader stays `alone` — serving and writable and NOT coordinating — so
    // it holds no election and needs no lease to write (ADR-0064's predicate is
    // `COORDINATING`). One node is a cluster here; what is being demonstrated is
    // the join, not the election, and the election has its own test above.
    {
        let db = tessaridb::Db::open(&stores[0]).unwrap();
        let joiner = tessari_types::RecordId::Uuid(ids[1]).to_string();
        db.session()
            .run(&format!(
                "DEFINE REPLICA joiner AT '{}' NODE '{joiner}' ROLES serving \
                 REPLICATES STORE; {WRITTEN}",
                JOIN[1].1
            ))
            .expect("a leader that knows who is joining it");
        drop(db);
    }

    // The newcomer's side, and this is the whole of what it is told. No replica
    // row, so its catalog names nobody and it cannot campaign, cannot route and
    // cannot collect — which is the circle the seed exists to break. `serving`
    // and not `writable`, because a node that collects while it also writes is
    // the divergence this design refuses everywhere else.
    {
        let db = tessaridb::Db::open(&stores[1]).unwrap();
        db.session()
            .run("DEFINE NODE ROLES serving;")
            .expect("a node that knows it is not the cluster");
        let peers = tessari_storage::Catalog::new(&mut db.store().begin().unwrap())
            .replicas()
            .unwrap();
        assert!(
            peers.is_empty(),
            "the joiner must be told nothing but an address, and its catalog \
             names {} peer(s)",
            peers.len()
        );
        drop(db);
    }

    let mut running = Vec::new();
    for (index, (client, peer)) in JOIN.iter().enumerate() {
        let (leaf, key, authority) = &papers[index];
        let mut command = Command::new(TESSARIDB);
        command
            .arg(&stores[index])
            .args(["--serve", client])
            .args(["--cluster-credential", leaf])
            .args(["--cluster-key", key])
            .args(["--cluster-authority", authority])
            .args(["--cluster-address", peer]);
        // Both nodes carry a seed, and only one of them will ever read it.
        //
        // `Told::from_parts` demands all five cluster parts, so a node cannot be
        // clustered without naming a seed — including the founding node, which
        // has nowhere to be reached FROM. That constraint became visible only
        // when the flag started being used; it is left standing (Q-577) because
        // it costs one flag and it means every node can be re-pointed, while
        // relaxing it would weaken the half-configuration check that exists to
        // stop a node coming up believing it has peers it cannot prove itself
        // to. The leader's seed is inert: its catalog names a peer, so the
        // bootstrap round is never the one that runs.
        let other = usize::from(index == 0);
        command.args([
            "--seed",
            &format!(
                "{}@{}",
                tessari_types::RecordId::Uuid(ids[other]),
                JOIN[other].1
            ),
        ]);
        let child = command
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        running.push(Running(child));
    }
    for (client, peer) in JOIN {
        assert!(listening(client, Duration::from_secs(30)), "{client}");
        assert!(listening(peer, Duration::from_secs(30)), "{peer}");
    }

    // One awareness round to greet the seed, one collection round to take the
    // log, and both cadences are ten seconds. Generous, because what is being
    // asserted is that it happens at all rather than how fast.
    let began = Instant::now();
    let mut held = None;
    let mut refusals = Vec::new();
    while began.elapsed() < Duration::from_secs(90) && held.is_none() {
        match counted(JOIN[1].0) {
            Ok(1) => held = Some(began.elapsed()),
            Ok(other) => refusals.push(format!("{other} record(s)")),
            Err(why) => refusals.push(why),
        }
        std::thread::sleep(POLL);
    }
    let held = held.unwrap_or_else(|| {
        panic!(
            "the joiner was given an address and never reached the leader's \
             records; the last thing it said was {:?}",
            refusals.last()
        )
    });
    assert!(
        held < Duration::from_secs(90),
        "the joiner took {held:?}, which is outside the window it was given"
    );

    // And it keeps receiving. The first collection is the join; whether the
    // node goes on being a member is a different question, and a node that took
    // the log once and then went quiet has joined nothing. The write goes over
    // the leader's client door because a running process holds its store from
    // the moment it started.
    {
        let mut client = Client::connect(JOIN[0].0).expect("the leader's client door");
        client
            .run(
                "USE NAMESPACE prod; USE DATABASE orders; CREATE item:2 = { n: 2 };",
                None,
            )
            .expect("a write on the one node that takes writes");
    }
    let since = Instant::now();
    let mut second = None;
    let mut silence = Vec::new();
    while since.elapsed() < Duration::from_secs(60) && second.is_none() {
        match counted(JOIN[1].0) {
            Ok(2) => second = Some(since.elapsed()),
            Ok(other) => silence.push(format!("{other} record(s)")),
            Err(why) => silence.push(why),
        }
        std::thread::sleep(POLL);
    }
    let second = second.unwrap_or_else(|| {
        panic!(
            "the joiner reached the leader's records in {held:?} and then \
             stopped: a record written after it joined never arrived, and the \
             last thing it said was {:?}",
            silence.last()
        )
    });
    assert!(
        second < Duration::from_secs(60),
        "the joiner took {second:?} to receive a write made after it joined"
    );

    // And the membership came with the records rather than from the flag. The
    // joiner wrote no replica row — that was asserted before it started — so a
    // row in its catalog now is one it collected.
    //
    // What that row does NOT do is spend the seed, and saying so here is the
    // correction W260 made: the row a cluster writes to admit a newcomer
    // describes the NEWCOMER, so this is the joiner reading its own name. It
    // names no peer to follow, `upstream` and `greet_round` both skip it, and a
    // bound that stopped dialling the seed on it left this node collecting
    // exactly once — which is what the second-write assertion above now
    // catches. The seed is spent when the catalog names somebody ELSE.
    drop(running);
    let db = tessaridb::Db::open(&stores[1]).expect("the joiner's store, once it has stopped");
    let peers = tessari_storage::Catalog::new(&mut db.store().begin().unwrap())
        .replicas()
        .unwrap();
    assert!(
        peers.iter().any(|peer| peer.name == "joiner"),
        "the joiner reached the leader's records in {held:?} and still does not \
         hold the membership row that was written before them"
    );
}

/// The lease period `address` reports, in whole seconds, or `None` for no policy.
///
/// Reads the field out of `cluster.failover` rather than searching the rendered
/// answer for a token. `Duration` derives `Debug`, so a rendered lease reads
/// `Duration { seconds: 41, nanos: 0 }` and a search for `41s` finds nothing —
/// which is a test that can only ever fail, and fails looking exactly like a
/// cluster that did not replicate.
fn lease_reported(address: &str) -> Result<Option<i64>, String> {
    let mut client = Client::connect(address).map_err(|why| why.to_string())?;
    let answers = client
        .run("INFO FOR NODE;", None)
        .map_err(|why| why.to_string())?;
    // `Answer::Value` and not `Answer::Records`: `INFO FOR NODE` answers one
    // object rather than a list of records, and the first version of this
    // helper matched the wrong variant — which made a correct report read as
    // *not a report* and failed the control assertion on a cluster that was
    // behaving perfectly.
    let Some(Answer::Value {
        value: tessari_types::Value::Object(report),
        ..
    }) = answers.last()
    else {
        return Err(format!("not a report: {answers:?}"));
    };
    let Some(tessari_types::Value::Object(cluster)) = report.get("cluster") else {
        return Err(format!("no cluster group: {report:?}"));
    };
    match cluster.get("failover") {
        Some(tessari_types::Value::Null) | None => Ok(None),
        Some(tessari_types::Value::Object(policy)) => match policy.get("lease") {
            Some(tessari_types::Value::Duration(span)) => Ok(Some(span.seconds())),
            other => Err(format!("no lease period: {other:?}")),
        },
        other => Err(format!("not a failover group: {other:?}")),
    }
}

/// G029 S2.2 against three operating-system processes.
///
/// The criterion's own method is *a live run in which one node's policy reaches
/// the other*, and until this wave nothing could originate a policy at all:
/// `Catalog::set_failover` had only test callers and the language had no
/// statement. `DEFINE FAILOVER` is that statement, and this is the run it was
/// written for.
///
/// # What makes this an observation rather than a restatement
///
/// The policy is written on the LEADER and read back on a node that never saw
/// the statement, through `INFO FOR NODE` — a different surface from the one
/// that wrote it, in a different process, against a store on a different disk.
/// Nothing in the assertion path touches the catalog directly, so a policy that
/// arrives is a policy that travelled along the log and by no other route.
///
/// # `null` before, and it is the control
///
/// `cluster.failover` is asserted ABSENT on the follower before the leader is
/// asked to set anything. That is what makes the later reading evidence: a
/// report that rendered the built-in defaults as a policy would satisfy the
/// second assertion while proving nothing, and the first assertion is what
/// forbids it.
///
/// # The second setting is the half a single write cannot show
///
/// One policy arriving proves a row replicated. It does not prove the ORDERING
/// works, because a follower with no policy accepts the first thing it is given
/// whatever the pair says. So the leader sets a second policy under the same
/// leadership, and the follower has to end on the later one — which is exactly
/// the case the version field exists for and the epoch alone cannot tell apart.
#[test]
#[ignore = "an election and two replication waits against three spawned \
            processes. It is the live validation G029 S2.2 names, and is run \
            explicitly: cargo test -p tessari-cli --test serving \
            a_failover_policy_set_on_the_leader -- --ignored"]
fn a_failover_policy_set_on_the_leader_reaches_a_node_that_never_saw_it() {
    let cluster = a_cluster_of_three(&PERIODS);
    let logs = cluster.logs.clone();
    let leader = the_node_a_majority_granted(&PERIODS);
    let follower = the_next_node(&PERIODS, leader);
    let surface = PERIODS[follower].0;
    let deciding = PERIODS[leader].0;
    let patience = Duration::from_secs(90);

    assert!(
        until(patience, || counted(surface) == Ok(1)),
        "the record never reached the follower, so nothing below would be \
         measuring a cluster.{}",
        what_the_nodes_said(&PERIODS, &logs)
    );

    // The control. A cluster nobody configured reports no policy, and this is
    // asserted before the leader is asked for one so that the reading after it
    // cannot be the defaults wearing a policy's clothes.
    assert_eq!(
        lease_reported(surface),
        Ok(None),
        "the follower reported a failover policy before one was ever set.{}",
        what_the_nodes_said(&PERIODS, &logs)
    );

    asked(
        deciding,
        "DEFINE FAILOVER AWARENESS 12s COLLECTION 11s ROUND 2s CAMPAIGN 3s \
         LEASE 41s;",
        None,
    )
    .expect("a leader sets the policy its cluster runs under");

    // 41 seconds and not a round number: the lease is the one period this test
    // chooses freely, and a value nothing else in the cluster uses cannot be
    // matched by a report that happened to render a default.
    assert!(
        until(patience, || lease_reported(surface) == Ok(Some(41))),
        "the policy never reached a node that did not write it.{}",
        what_the_nodes_said(&PERIODS, &logs)
    );

    // The ordering half. A second policy under the same leadership carries the
    // next version, and the follower must end on the later one — a replica that
    // applied writes in arrival order rather than by the pair would be
    // indistinguishable from a correct one until exactly this case.
    asked(
        deciding,
        "DEFINE FAILOVER AWARENESS 12s COLLECTION 11s ROUND 2s CAMPAIGN 3s \
         LEASE 43s;",
        None,
    )
    .expect("a leader may set the policy again under one leadership");

    // Reading the field rather than searching a rendering is what makes this
    // assertion do both halves at once: the later policy is present AND the
    // earlier one is gone, because there is exactly one lease to read. A token
    // search would have needed a second assertion for the absence, and the
    // first version of this test was written that way and could not have
    // worked at all — `Duration` derives `Debug`, so a rendered answer spells a
    // lease `Duration { seconds: 41, nanos: 0 }` and the token `41s` appears
    // nowhere in it. The instrument, not the cluster.
    assert!(
        until(patience, || lease_reported(surface) == Ok(Some(43))),
        "the later policy never replaced the earlier one on the follower.{}",
        what_the_nodes_said(&PERIODS, &logs)
    );
}

/// S3.2's band — 47887-47890, clean: 47881-47886 belong to the three-node
/// failover cluster and 47891-47894 to the join above.
const REFUSING: [(&str, &str); 2] = [
    ("127.0.0.1:47887", "127.0.0.1:47888"),
    ("127.0.0.1:47889", "127.0.0.1:47890"),
];

/// What the joiner wrote before anybody pointed it at a cluster.
///
/// `REPLICATION NONE` so nothing about this namespace asks to be replicated:
/// what is being measured is the address it occupies, not a subscription.
const ITS_OWN: &str = "DEFINE NAMESPACE research REPLICATION NONE; USE NAMESPACE research; \
                       DEFINE DATABASE notebooks; USE DATABASE notebooks; \
                       DEFINE COLLECTION note; CREATE note:1 = { n: 1 };";

/// Every namespace a store on disk can name, opened offline.
fn namespaces_on(store: &std::path::Path) -> Vec<String> {
    let db = tessaridb::Db::open(store).expect("the store opens");
    let mut transaction = db.store().begin().expect("a read");
    let names = tessari_storage::Catalog::new(&mut transaction)
        .namespaces()
        .expect("the catalog answers")
        .into_iter()
        .map(|namespace| namespace.name)
        .collect();
    transaction.rollback();
    names
}

/// S3.2 — a join is refused while the joining node holds a tenancy of its own,
/// and the refusal says so in words an operator can act on.
///
/// # What is being defended, and why it needed a refusal rather than a repair
///
/// W391 measured it in one process: two stores that each declared a first
/// namespace both hold namespace 1, so applying the cluster's log replaces the
/// definition at the address the joiner's records are filed under. Nothing is
/// deleted, no row count moves and nothing reaches the log — every record is
/// simply read afterwards through somebody else's name, schema, replication
/// class and grants. ADR-0077's remedy does not generalise to it: a membership
/// row could be keyed by the name it carries because nothing pointed at its
/// number, while a namespace id IS the address a record lives at.
///
/// # Why the second half kills the node instead of asking it nicely
///
/// The joiner runs `ROLES serving`, so a write arriving at its client door is
/// forwarded to a writable peer — and it has declared none, which is the whole
/// point of a node that has not joined yet. So the operator's remedy is taken
/// with the store offline, which is also what every system this was ranked
/// against requires: Elasticsearch's `detach-cluster` and Kafka's
/// `meta.properties` are both stop-the-node operations.
///
/// The restart is load-bearing beyond convenience: it proves the refusal is a
/// function of what the store HOLDS rather than a decision remembered from the
/// first attempt.
#[test]
#[ignore = "two spawned processes and two collection cadences; run it with \
            cargo test -p tessari-cli --test serving a_join_that -- --ignored"]
fn a_join_that_would_reinterpret_a_tenancy_is_refused_until_the_operator_removes_it() {
    let directory = tempfile::tempdir().unwrap();
    let minted = Minted::new();

    let mut stores = Vec::new();
    let mut ids = Vec::new();
    let mut papers = Vec::new();
    for (index, _) in REFUSING.iter().enumerate() {
        let home = directory.path().join(format!("r{index}"));
        std::fs::create_dir_all(&home).unwrap();
        let store = home.join("store");
        let db = tessaridb::Db::open(&store).unwrap();
        let id = db.store().node_identity().unwrap().id;
        drop(db);
        papers.push(credentials(&minted, id, &home));
        ids.push(id);
        stores.push(store);
    }

    {
        let db = tessaridb::Db::open(&stores[0]).unwrap();
        let joiner = tessari_types::RecordId::Uuid(ids[1]).to_string();
        db.session()
            .run(&format!(
                "DEFINE REPLICA joiner AT '{}' NODE '{joiner}' ROLES serving \
                 REPLICATES STORE; {WRITTEN}",
                REFUSING[1].1
            ))
            .expect("a leader that knows who is joining it");
        drop(db);
    }
    {
        let db = tessaridb::Db::open(&stores[1]).unwrap();
        db.session()
            .run(&format!("DEFINE NODE ROLES serving; {ITS_OWN}"))
            .expect("a node with a tenancy of its own");
        drop(db);
    }
    // Both stores really do file their first namespace at the same address —
    // asserted rather than assumed, because a build that numbered them
    // differently would make every assertion below pass for the wrong reason.
    assert_eq!(
        namespaces_on(&stores[0]),
        vec!["prod".to_owned()],
        "the cluster's tenancy"
    );
    assert_eq!(
        namespaces_on(&stores[1]),
        vec!["research".to_owned()],
        "and the joiner's own"
    );

    // Each node's seed names the OTHER one: a seed carries a node id as well as
    // an address (ADR-0067), because the handshake derives the peer's TLS name
    // from its id. The leader's is inert — its catalog already names a peer.
    let seeds: Vec<String> = (0..REFUSING.len())
        .map(|index| {
            let other = usize::from(index == 0);
            format!(
                "{}@{}",
                tessari_types::RecordId::Uuid(ids[other]),
                REFUSING[other].1
            )
        })
        .collect();

    let mut logs = Vec::new();
    let mut running = Vec::new();
    for index in 0..REFUSING.len() {
        let (log, child) = spawn_refusing(directory.path(), &stores, &papers, &seeds, index);
        logs.push(log);
        running.push(child);
    }
    for (client, peer) in REFUSING {
        assert!(listening(client, Duration::from_secs(30)), "{client}");
        assert!(listening(peer, Duration::from_secs(30)), "{peer}");
    }

    // The refusal, in the joiner's own log, in the words an operator reads.
    // Both tokens: the NAME of what would be reinterpreted, without which the
    // message names no object, and the statement that lifts it, without which
    // it names no remedy.
    let patience = Duration::from_secs(90);
    assert!(
        until(patience, || {
            let said = std::fs::read_to_string(&logs[1]).unwrap_or_default();
            said.contains("research") && said.contains("DROP NAMESPACE")
        }),
        "the joiner collected, or refused without saying what would be \
         reinterpreted.{}",
        what_the_nodes_said(&REFUSING, &logs)
    );

    // Refused means nothing arrived. The node is stopped first because a
    // running process holds its store.
    drop(running.pop().expect("the joiner is running"));
    assert_eq!(
        namespaces_on(&stores[1]),
        vec!["research".to_owned()],
        "the refusal must leave the joiner exactly as it was — the cluster's \
         namespace arriving here is the destruction this criterion is about.{}",
        what_the_nodes_said(&REFUSING, &logs)
    );
    {
        let db = tessaridb::Db::open(&stores[1]).unwrap();
        let answers = db
            .session()
            .run("USE NAMESPACE research; USE DATABASE notebooks; SELECT * FROM note;")
            .expect("the joiner's own tenancy still reads");
        assert!(
            matches!(
                answers.last(),
                Some(tessari_session::Outcome::Records { records, .. }) if records.len() == 1
            ),
            "and its own record is still under it: {answers:?}"
        );
        drop(db);
    }

    // The destruction, stated by being performed. The language walks the
    // operator down their own tree — a namespace will not drop while a database
    // is under it, and a database will not drop while a table is — so every
    // object destroyed is one they named.
    {
        let db = tessaridb::Db::open(&stores[1]).unwrap();
        // The role comes first, and finding that out is what this half of the
        // test is for: the joiner is configured `ROLES serving`, `Effect::admits`
        // refuses a write on a node without `Roles::WRITABLE`, and every
        // statement below is a write — so the remedy the refusal names is itself
        // refused on the one node that needs it. `DEFINE NODE` is classified as
        // a READ (a local `META` write, ADR-0018), which is the only reason this
        // is a detour rather than a dead end. The refusal says so in its own
        // words; this asserts the sequence it names actually runs.
        db.session()
            .run("DEFINE NODE ROLES writable;")
            .expect("a serving node can always restore its own write authority");
        db.session()
            .run("USE NAMESPACE research; USE DATABASE notebooks; DROP TABLE note;")
            .expect("the operator drops their table");
        db.session()
            .run("USE NAMESPACE research; DROP DATABASE notebooks;")
            .expect("then the database");
        db.session()
            .run("DROP NAMESPACE research;")
            .expect("then the namespace the cluster's would have replaced");
        db.session().run("DEFINE NODE ROLES serving;").expect(
            "and the role goes back, because a node that collects while \
                     it also writes is the divergence this design refuses",
        );
        drop(db);
    }
    assert!(
        namespaces_on(&stores[1]).is_empty(),
        "the joiner holds no tenancy of its own"
    );

    let (log, child) = spawn_refusing(directory.path(), &stores, &papers, &seeds, 1);
    logs[1] = log;
    running.push(child);
    assert!(listening(REFUSING[1].0, Duration::from_secs(30)));

    let began = Instant::now();
    let mut held = None;
    let mut refusals = Vec::new();
    while began.elapsed() < patience && held.is_none() {
        match counted(REFUSING[1].0) {
            Ok(1) => held = Some(began.elapsed()),
            Ok(other) => refusals.push(format!("{other} record(s)")),
            Err(why) => refusals.push(why),
        }
        std::thread::sleep(POLL);
    }
    held.unwrap_or_else(|| {
        panic!(
            "a node with no tenancy of its own must collect; the last thing it \
             said was {:?}.{}",
            refusals.last(),
            what_the_nodes_said(&REFUSING, &logs)
        )
    });
}

/// One node of [`REFUSING`], with its standard error kept in a file of its own.
fn spawn_refusing(
    directory: &std::path::Path,
    stores: &[std::path::PathBuf],
    papers: &[(String, String, String)],
    seeds: &[String],
    index: usize,
) -> (std::path::PathBuf, Running) {
    let (leaf, key, authority) = &papers[index];
    let log = directory.join(format!("r{index}")).join("node.log");
    let writing = std::fs::File::create(&log).unwrap();
    let child = Command::new(TESSARIDB)
        .arg(&stores[index])
        .args(["--serve", REFUSING[index].0])
        .args(["--cluster-credential", leaf])
        .args(["--cluster-key", key])
        .args(["--cluster-authority", authority])
        .args(["--cluster-address", REFUSING[index].1])
        .args(["--seed", &seeds[index]])
        .stdout(Stdio::null())
        .stderr(Stdio::from(writing))
        .spawn()
        .unwrap();
    (log, Running(child))
}
