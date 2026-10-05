use std::io::{Read, Write};
use std::net::TcpStream;
use std::sync::Arc;
use std::time::Instant;

use tessari_encoding::{NODE_ID_LEN, NodeVersion, Roles};
use tessari_types::{Epoch, Sequence};
use tessaridb::{Db, Parameters};

use super::Node;
use crate::directory::Directory;
use crate::driver::Published;
use crate::frame;
use crate::message::Request;
use crate::peer::Hello;

/// A node that cannot answer a bounded read, beside one that can.
///
/// The same arrangement `talking.rs` builds for the happy path, and it is
/// built again here rather than shared because that file cannot reach the
/// crate-private frame vocabulary this test is written in — the whole point
/// of the test is to speak the protocol as a client of an older build would,
/// which no `Client` in this workspace will ever do again.
fn a_node_that_must_redirect() -> String {
    a_node_whose_peer_claims(Roles::SERVING)
}

/// The same node, with the peer claiming `roles` instead.
///
/// One fixture and not two, because the two redirects differ only in what
/// sends them: the staleness axis needs a peer that merely serves, and the
/// authority axis needs one that says it writes. Everything after that —
/// the drained local node, the port, the thread — is the same setup, and a
/// copy of it would be a second place for the setup to drift.
fn a_node_whose_peer_claims(roles: Roles) -> String {
    let mut directory = Directory::new();
    directory.heard(
        "two.example:9080",
        Hello {
            node: [3; NODE_ID_LEN],
            build: NodeVersion {
                major: 0,
                minor: 1,
                patch: 1,
            },
            epoch: Epoch::new(1),
            roles,
            tail: Sequence::new(4096),
            tail_leadership: Epoch::new(1),
            current_as_of: Some(std::time::Duration::from_secs(1)),
            policy: None,
            line: None,
        },
        Instant::now(),
    );

    let db = Db::in_memory().expect("an in-memory store");
    {
        // Schema first, role second: a node that may not write cannot define
        // a collection either. Holding somebody else's writes is what puts
        // this node's own copy outside every bound.
        let mut session = db.session();
        session
            .run(
                "DEFINE NAMESPACE prod; USE NAMESPACE prod; DEFINE DATABASE orders; \
                     USE DATABASE orders; DEFINE COLLECTION users;",
            )
            .expect("the schema");
        session
            .run("DEFINE NODE ROLES serving;")
            .expect("the role that stops this node writing");
    }

    let node = Node::bind(Arc::new(db), "127.0.0.1:0")
        .expect("a loopback port")
        .among(Arc::new(Published::holding(directory)));
    let address = node.address().expect("the port it took");
    drop(std::thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .expect("a runtime");
        drop(runtime.block_on(node.serve(tokio_util::sync::CancellationToken::new())));
    }));
    address
}

/// Greet as a build of `MAJOR.minor`, and hear the node's greeting back.
///
/// Written out as six bytes rather than through `frame::greet`, because that
/// function sends whatever this build's `MINOR` happens to be — which is the
/// value under test, so using it would make the test agree with itself.
fn greet_as(stream: &mut TcpStream, minor: u8) {
    stream.write_all(b"TESS").expect("the magic");
    stream.write_all(&[frame::MAJOR, minor]).expect("a version");
    stream.flush().expect("the greeting");
    let mut theirs = [0_u8; 6];
    stream.read_exact(&mut theirs).expect("a greeting back");
    assert_eq!(&theirs[..4], b"TESS");
}

/// Ask for the bounded read, and answer with the tag that came back.
fn tag_answering_a_bounded_read(minor: u8) -> u8 {
    tag_answering(
        a_node_that_must_redirect(),
        minor,
        "USE NAMESPACE prod; USE DATABASE orders; SELECT * FROM users STALENESS 60s;",
    )
}

/// Run `script` against `address` as a build of `MAJOR.minor`, and answer
/// with the FRAME TAG that came back — the byte itself, off the socket,
/// rather than whatever a client would have decoded it into.
///
/// That distinction is the whole point of these cases. A redirect that
/// arrived as a refusal would still reach a caller as an error carrying an
/// address, and every assertion made through a client would go on passing.
fn tag_answering(address: String, minor: u8, script: &str) -> u8 {
    let mut stream = TcpStream::connect(&address).expect("the node this test started");
    greet_as(&mut stream, minor);

    let body = Request {
        script: script.to_owned(),
        credentials: None,
        parameters: Parameters::new(),
    }
    .encode();
    let length = u32::try_from(body.len()).expect("a script smaller than four gibibytes");
    stream
        .write_all(&[frame::Kind::Request.tag()])
        .expect("the tag");
    stream.write_all(&length.to_be_bytes()).expect("the length");
    stream.write_all(&body).expect("the request");
    stream.flush().expect("the request to leave");

    let mut header = [0_u8; 5];
    stream.read_exact(&mut header).expect("an answer");
    header[0]
}

/// Send `script` on an open connection and answer with the frame that came back.
fn ask(stream: &mut TcpStream, script: &str) -> (u8, Vec<u8>) {
    let body = Request {
        script: script.to_owned(),
        credentials: None,
        parameters: Parameters::new(),
    }
    .encode();
    let length = u32::try_from(body.len()).expect("a short script");
    stream
        .write_all(&[frame::Kind::Request.tag()])
        .expect("the tag");
    stream.write_all(&length.to_be_bytes()).expect("the length");
    stream.write_all(&body).expect("the request");
    stream.flush().expect("the request to leave");
    let mut header = [0_u8; 5];
    stream.read_exact(&mut header).expect("an answer");
    let told = u32::from_be_bytes([header[1], header[2], header[3], header[4]]);
    let mut answer = vec![0_u8; usize::try_from(told).expect("a length that fits")];
    stream.read_exact(&mut answer).expect("the answer's body");
    (header[0], answer)
}

/// Send one request carrying `credentials`, and answer with the tag that came back.
fn ask_as(stream: &mut TcpStream, script: &str, credentials: Option<(&str, &str)>) -> u8 {
    let body = Request {
        script: script.to_owned(),
        credentials: credentials.map(|(name, password)| (name.to_owned(), password.to_owned())),
        parameters: Parameters::new(),
    }
    .encode();
    frame::write(stream, frame::Kind::Request, &body).expect("the request");
    frame::read(stream)
        .expect("an answer")
        .expect("an answer, not a hang-up")
        .0
        .tag()
}

#[test]
fn a_node_on_one_worker_answers_while_a_slow_statement_runs() {
    // S11: no store call runs on a runtime worker. One worker, and a sign-in
    // that costs a password hash on the store's side: a client arriving
    // while it runs is answered first. A statement run on the worker would
    // hold it, and nothing else on the node could even be read until the
    // hash was done.
    const PASSWORD: &str = "correct horse battery";
    let db = Arc::new(Db::in_memory().expect("an in-memory store"));
    db.session()
        .run(&format!(
            "DEFINE USER root ROLE owner PASSWORD '{PASSWORD}';"
        ))
        .expect("the store closed");
    let node = Node::bind(db, "127.0.0.1:0").expect("a loopback port");
    let address = node.address().expect("the port it took");
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .enable_all()
        .build()
        .expect("a one-worker runtime");
    drop(std::thread::spawn(move || {
        drop(runtime.block_on(node.serve(tokio_util::sync::CancellationToken::new())));
    }));

    let mut quick = TcpStream::connect(&address).expect("the node this test started");
    greet_as(&mut quick, frame::MINOR);
    let mut slow = TcpStream::connect(&address).expect("the node this test started");
    greet_as(&mut slow, frame::MINOR);
    // The sign-in is on the wire before the other client asks, and is given
    // a few milliseconds to reach its hash — which takes ~15 ms here.
    let body = Request {
        script: "INFO FOR NODE;".to_owned(),
        credentials: Some(("root".to_owned(), PASSWORD.to_owned())),
        parameters: Parameters::new(),
    }
    .encode();
    frame::write(&mut slow, frame::Kind::Request, &body).expect("the sign-in");
    let hashing = std::thread::spawn(move || {
        let answered = frame::read(&mut slow)
            .expect("an answer")
            .expect("an answer, not a hang-up");
        (answered.0.tag(), Instant::now())
    });
    std::thread::sleep(std::time::Duration::from_millis(5));
    // Anonymous on a closed store: refused by the session, with no hash.
    let tag = ask_as(&mut quick, "INFO FOR NODE;", None);
    let quick_at = Instant::now();
    assert_eq!(
        tag,
        frame::Kind::Refusal.tag(),
        "an anonymous statement was not refused"
    );
    let (tag, slow_at) = hashing.join().expect("the sign-in's thread");
    assert_eq!(
        tag,
        frame::Kind::Answer.tag(),
        "the owner's sign-in was refused"
    );
    assert!(
        quick_at < slow_at,
        "the node answered nobody while one statement hashed a password"
    );
}

#[test]
fn a_statement_refused_as_busy_keeps_the_session_it_arrived_in() {
    let db = Arc::new(Db::in_memory().expect("an in-memory store"));
    let mut node = Node::bind(db, "127.0.0.1:0").expect("a loopback port");
    // One slot, so the test can take the whole bridge by holding it — the
    // resource is taken, not raced for.
    node.bridge = Arc::new(tessari_serve::Bridge::new(1));
    let bridge = node.bridge();
    let address = node.address().expect("the port it took");
    let runtime = Arc::new(
        tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .expect("a runtime"),
    );
    let serving = Arc::clone(&runtime);
    drop(std::thread::spawn(move || {
        drop(serving.block_on(node.serve(tokio_util::sync::CancellationToken::new())));
    }));

    let mut stream = TcpStream::connect(&address).expect("the node this test started");
    greet_as(&mut stream, frame::MINOR);
    let (tag, body) = ask(
        &mut stream,
        "DEFINE NAMESPACE shop; USE NAMESPACE shop; DEFINE DATABASE orders; \
             USE DATABASE orders; DEFINE COLLECTION items;",
    );
    assert_eq!(
        tag,
        frame::Kind::Answer.tag(),
        "the setup was refused: {}",
        String::from_utf8_lossy(&body)
    );

    // Hold the only slot with a call that waits for the test to let go.
    let (release, held) = std::sync::mpsc::channel::<()>();
    let (taken, slot_is_held) = std::sync::mpsc::channel::<()>();
    let holding = runtime.spawn(async move {
        bridge
            .call((), move |()| {
                taken.send(()).expect("the test waiting for the slot");
                held.recv().expect("the test letting go");
            })
            .await
    });
    slot_is_held.recv().expect("the slot taken");

    let (tag, body) = ask(&mut stream, "SELECT * FROM items;");
    assert_eq!(tag, frame::Kind::Refusal.tag(), "a full bridge answered");
    assert_eq!(
        body,
        frame::refusal(
            frame::MINOR,
            tessari_types::RefusalClass::Unavailable,
            super::conversation::BUSY
        ),
        "refused, but not for being busy, or without its class"
    );

    release.send(()).expect("the held call");
    drop(runtime.block_on(holding));
    let (tag, body) = ask(&mut stream, "SELECT * FROM items;");
    assert_eq!(
        tag,
        frame::Kind::Answer.tag(),
        "the refused statement lost the session: {}",
        String::from_utf8_lossy(&body)
    );
}

#[test]
fn a_client_that_can_read_a_redirect_is_sent_one() {
    assert_eq!(
        tag_answering_a_bounded_read(frame::REDIRECTS),
        frame::Kind::Elsewhere.tag(),
        "the node had somewhere to send this read and refused instead"
    );
}

#[test]
fn a_read_that_named_the_leader_leaves_as_a_redirect_and_not_a_refusal() {
    // The authority axis reaching the wire. It is the same frame the
    // staleness axis already sends, deliberately: one concept, one tag, one
    // arm in each transport — a second variant would be a second arm here
    // and in HTTP, where forgetting one answers a redirect as a plain
    // refusal with nothing anywhere in an error state.
    assert_eq!(
        tag_answering(
            a_node_whose_peer_claims(Roles::SERVING.and(Roles::WRITABLE)),
            frame::REDIRECTS,
            "USE NAMESPACE prod; USE DATABASE orders; \
                 SELECT * FROM users ANSWERED BY LEADER;",
        ),
        frame::Kind::Elsewhere.tag(),
        "this node knew of a peer that claims to write and refused instead"
    );
}

#[test]
fn a_read_that_named_the_leader_with_no_leader_to_name_is_refused() {
    // The other half, and the one that must NOT be a redirect: every peer
    // here merely serves. A node that sent tag 13 anyway would be naming a
    // follower as the leader, which is the quiet wrong answer the whole
    // clause exists to prevent.
    assert_eq!(
        tag_answering(
            a_node_whose_peer_claims(Roles::SERVING),
            frame::REDIRECTS,
            "USE NAMESPACE prod; USE DATABASE orders; \
                 SELECT * FROM users ANSWERED BY LEADER;",
        ),
        frame::Kind::Refusal.tag(),
        "a read was sent to a peer that never claimed to write"
    );
}

#[test]
fn a_client_from_before_the_redirect_existed_is_refused_rather_than_confused() {
    // The minor's whole job: what this side may SEND to an older peer. A
    // build that predates tag 13 cannot name the frame, and would have to
    // decide whether an unknown tag is corruption — which is a worse answer
    // than the refusal it has always had.
    assert_eq!(
        tag_answering_a_bounded_read(0),
        frame::Kind::Refusal.tag(),
        "a client that cannot name tag 13 was sent tag 13"
    );
}

#[test]
fn a_refusal_carries_its_class_to_a_client_that_can_read_one_and_only_its_words_to_an_older_one() {
    // ADR-0117 D3: the body has no length prefix, so a client before 1.3 would
    // read a class byte as the first letter of the message. It is sent the
    // words exactly as before; a client of 1.3 gets the class first, and the
    // same words after it.
    let address = a_node_that_must_redirect();
    let refused_as = |minor: u8, script: &str| {
        let mut stream = TcpStream::connect(&address).expect("the node this test started");
        greet_as(&mut stream, minor);
        let (tag, body) = ask(&mut stream, script);
        assert_eq!(tag, frame::Kind::Refusal.tag(), "{script} was answered");
        body
    };
    for (script, class) in [
        ("SELECT FROM;", tessari_types::RefusalClass::Invalid),
        // This node serves and does not write, and no peer link can carry the
        // write elsewhere: the request was fine, this node is the wrong place.
        (
            "USE NAMESPACE prod; USE DATABASE orders; DEFINE COLLECTION more;",
            tessari_types::RefusalClass::Unavailable,
        ),
    ] {
        let older = refused_as(2, script);
        assert!(
            older
                .first()
                .is_some_and(|first| !frame::CLASS_BYTES.contains(first)),
            "{script}: an older client was sent a class byte"
        );
        let current = refused_as(3, script);
        assert_eq!(current.first(), Some(&class.byte()), "{script}");
        assert_eq!(
            current.get(1..),
            Some(older.as_slice()),
            "{script}: the words differ"
        );
        let (read, words) = frame::read_refusal(&current);
        assert_eq!(read, Some(Some(class)));
        assert_eq!(words.as_bytes(), older.as_slice());
        assert_eq!(
            frame::read_refusal(&older).0,
            None,
            "an older body read as classed"
        );
    }
}
