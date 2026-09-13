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

use tessari_wire::{Answer, Client};

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
    let directory = tempfile::tempdir().unwrap();
    let minted = Minted::new();

    // Each node's id is generated by its own store on first open, so it has to
    // be read before anything can declare it. Its own directory each, because
    // `credentials` writes three fixed filenames.
    let mut stores = Vec::new();
    let mut ids = Vec::new();
    let mut papers = Vec::new();
    for index in 0..CLUSTER.len() {
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

    // Each node declares the other two and never itself. The membership is
    // `peers.len() + 1` (`campaign.rs`), so a self-row would make it four,
    // needing three grants, and the cluster would stop electing on the first
    // loss —
    // which is the exact case this test exists to exercise.
    //
    // `REPLICATES STORE` rather than the namespace the criterion names, and the
    // reason is a property of the engine rather than a convenience: a namespace
    // subscription resolves to a namespace **id** when the row is written, so
    // the name has to exist on the granting node first. ADR-0063 means any of
    // the three may win, so no node can be pinned as the granter before the
    // election. The namespace is narrowed onto one follower below, once it
    // exists and has an id every node agrees on.
    for (index, store) in stores.iter().enumerate() {
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
        let mut script = String::new();
        for other in 0..CLUSTER.len() {
            if other == index {
                continue;
            }
            let named = tessari_types::RecordId::Uuid(ids[other]).to_string();
            script.push_str(&format!(
                " DEFINE REPLICA n{other} AT '{}' NODE '{named}' \
                  ROLES serving, coordinating REPLICATES STORE;",
                CLUSTER[other].1
            ));
        }
        script.push_str(" DEFINE NODE ROLES serving, writable, coordinating;");
        db.session()
            .run(&script)
            .expect("a cluster of three, declared");
        drop(db);
    }

    let mut running: Vec<Option<Running>> = Vec::new();
    for index in 0..CLUSTER.len() {
        let (leaf, key, authority) = &papers[index];
        let child = Command::new(TESSARIDB)
            .arg(&stores[index])
            .args(["--serve", CLUSTER[index].0])
            .args(["--cluster-credential", leaf])
            .args(["--cluster-key", key])
            .args(["--cluster-authority", authority])
            .args(["--cluster-address", CLUSTER[index].1])
            // A seed names the node as well as the address (ADR-0067): the
            // handshake derives the peer's TLS name from its id, so a bare
            // address is not a dial this transport can express. These three
            // declare each other in their catalogs, so the seed is never read
            // — it is here because a running node takes the flag.
            .args([
                "--seed",
                &format!(
                    "{}@{}",
                    tessari_types::RecordId::Uuid(ids[(index + 1) % CLUSTER.len()]),
                    CLUSTER[(index + 1) % CLUSTER.len()].1
                ),
            ])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        running.push(Some(Running(child)));
    }
    for (client, peer) in CLUSTER {
        assert!(listening(client, Duration::from_secs(30)), "{client}");
        assert!(listening(peer, Duration::from_secs(30)), "{peer}");
    }

    // Wait for the first epoch to be granted, by trying to use it. A write
    // before any round concludes is refused on every node — that is ADR-0064
    // working, not a defect, and it is why this polls rather than writing once.
    let began = Instant::now();
    let mut elected = None;
    let mut refusals = [String::new(), String::new(), String::new()];
    while began.elapsed() < Duration::from_secs(90) && elected.is_none() {
        for (index, (surface, _)) in CLUSTER.iter().enumerate() {
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
    let leader = elected.unwrap_or_else(|| {
        panic!(
            "no node accepted a write in ninety seconds. A cluster of three \
             that elects nobody has no writer anywhere, which is what ADR-0064 \
             made possible and what an election is supposed to resolve. The \
             last refusal from each node: {refusals:?}"
        )
    });

    let follower = (0..CLUSTER.len()).find(|index| *index != leader).unwrap();

    // The record reaches a node that never wrote it. Until this holds there is
    // no replication to lose, so a failover asserted before it would be a
    // failover of nothing.
    let began = Instant::now();
    let mut replicated = false;
    while began.elapsed() < Duration::from_secs(90) {
        if counted(CLUSTER[follower].0) == Ok(1) {
            replicated = true;
            break;
        }
        std::thread::sleep(POLL);
    }
    assert!(
        replicated,
        "the follower never received the leader's record, so nothing below \
         would be measuring a cluster"
    );

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
    let successor = successor.expect(
        "the cluster lost its leader and never got another: two of three nodes \
         were up, which is a majority, so a round could have been carried",
    );
    assert_ne!(successor, leader, "the killed node cannot be the successor");

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

    // And the membership came with the records rather than from the flag. The
    // joiner wrote no replica row — that was asserted before it started — so a
    // row in its catalog now is one it collected. This is what makes the seed
    // spent: from here the catalog answers who the members are, and the address
    // on the command line is never read again.
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
