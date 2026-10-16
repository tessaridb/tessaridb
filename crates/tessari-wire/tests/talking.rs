//! A client and a node, over a real socket.
//!
//! These are the tests the unit ones cannot be: that a value survives the
//! network as the value it was, that a hostile frame closes one connection and
//! not the node, and that the store's own refusals arrive as refusals rather
//! than as silence.

#![allow(clippy::panic, clippy::unwrap_used, clippy::indexing_slicing)]
// `expect_used` and `as_conversions` govern production code; a test states its own expectations.
#![allow(clippy::expect_used, clippy::as_conversions)]

use std::io::Write;
use std::net::TcpStream;
use std::sync::Arc;
use tessari_session::RunNow;

use tessari_wire::{Answer, Client, Node};
use tessaridb::{Db, Value};

/// A node on a loopback port the operating system picked, plus its address.
fn serving(db: Db) -> (Arc<Node>, String) {
    started(Node::bind(Arc::new(db), "127.0.0.1:0").unwrap())
}

/// The same, among the peers `elsewhere` knows about.
fn serving_among(db: Db, elsewhere: Arc<tessari_wire::Published>) -> (Arc<Node>, String) {
    started(
        Node::bind(Arc::new(db), "127.0.0.1:0")
            .unwrap()
            .among(elsewhere),
    )
}

fn started(node: Node) -> (Arc<Node>, String) {
    let node = Arc::new(node);
    let address = node.address().unwrap();
    let held = Arc::clone(&node);
    drop(std::thread::spawn(move || serve_until_the_test_ends(&held)));
    (node, address)
}

const READY: &str = "DEFINE NAMESPACE prod; USE NAMESPACE prod; \
                     DEFINE DATABASE orders; USE DATABASE orders;";

#[test]
fn a_script_runs_and_answers_one_outcome_per_statement() {
    let (_node, address) = serving(Db::in_memory().unwrap());
    let mut client = Client::connect(&address).unwrap();

    let answers = client
        .run(
            &format!("{READY} DEFINE COLLECTION users; CREATE users:1 = {{ name: 'ada' }}; SELECT * FROM users;"),
            None,
        )
        .unwrap();
    assert_eq!(answers.len(), 7);
    match &answers[6] {
        Answer::Records { records, path, .. } => {
            assert_eq!(records.len(), 1);
            assert_eq!(records[0].0, "1");
            assert_eq!(path, "scan");
        }
        other => panic!("not records: {other:?}"),
    }
}

#[test]
fn a_value_crosses_the_network_as_the_value_it_was() {
    // The whole reason this protocol exists rather than the console reading the
    // HTTP endpoint. Every kind that JSON would have flattened into a string
    // comes back as itself, so a client decides nothing.
    let (_node, address) = serving(Db::in_memory().unwrap());
    let mut client = Client::connect(&address).unwrap();
    client
        .run(&format!("{READY} DEFINE COLLECTION probe;"), None)
        .unwrap();
    client
        .run(
            "USE NAMESPACE prod; USE DATABASE orders; \
             CREATE probe:1 = { exact: dec 12.34, real: 1.5, whole: 42, span: 1h30m, \
             at: datetime '2026-01-15T09:30:00Z', raw: 0x0a1b, \
             who: uuid '00112233-4455-6677-8899-aabbccddeeff', \
             list: [1, 'two'], nested: { inner: 1 }, absent: NONE, empty: NULL };",
            None,
        )
        .unwrap();

    let answers = client
        .run(
            "USE NAMESPACE prod; USE DATABASE orders; SELECT * FROM probe:1;",
            None,
        )
        .unwrap();
    let Answer::Records { records, .. } = &answers[2] else {
        panic!("not records: {:?}", answers[2]);
    };
    let Value::Object(held) = &records[0].1 else {
        panic!("not an object");
    };

    // A decimal is a decimal, not a quoted string a client has to decide about.
    assert!(matches!(
        held.get("exact"),
        Some(Value::Number(tessaridb::Number::Decimal(_)))
    ));
    assert!(matches!(
        held.get("real"),
        Some(Value::Number(tessaridb::Number::Float(_)))
    ));
    assert!(matches!(
        held.get("whole"),
        Some(Value::Number(tessaridb::Number::Integer(42)))
    ));
    assert!(matches!(held.get("span"), Some(Value::Duration(_))));
    assert!(matches!(held.get("at"), Some(Value::Datetime(_))));
    assert!(matches!(held.get("raw"), Some(Value::Bytes(_))));
    assert!(matches!(held.get("who"), Some(Value::Uuid(_))));
    assert!(matches!(held.get("list"), Some(Value::Array(_))));
    assert!(matches!(held.get("nested"), Some(Value::Object(_))));
    // `none` and `null` are two values and arrive as two values. JSON has one
    // word for both, so the HTTP surface carries the distinction by whether it
    // writes the key at all; here there is nothing to carry it with, because
    // nothing was lost.
    assert_eq!(held.get("absent"), Some(&Value::None));
    assert_eq!(held.get("empty"), Some(&Value::Null));
}

#[test]
fn a_connection_remembers_what_it_selected() {
    // The difference between a connection and a sequence of unrelated sessions
    // that happen to share a socket. Without this a prompt over this protocol
    // would have to re-say where it was in every single statement.
    let (_node, address) = serving(Db::in_memory().unwrap());
    let mut client = Client::connect(&address).unwrap();

    client.run(READY, None).unwrap();
    // No `USE` here at all: the selection above is still in force.
    client.run("DEFINE COLLECTION users;", None).unwrap();
    let answers = client
        .run(
            "CREATE users:1 = { name: 'ada' }; SELECT * FROM users;",
            None,
        )
        .unwrap();

    let Answer::Records { records, .. } = &answers[1] else {
        panic!("not records: {:?}", answers[1]);
    };
    assert_eq!(records.len(), 1);
}

#[test]
fn a_second_connection_starts_where_every_connection_starts() {
    // The memory above belongs to the connection and not to the store, which is
    // what makes two clients independent rather than two views of one session.
    let (_node, address) = serving(Db::in_memory().unwrap());
    let mut first = Client::connect(&address).unwrap();
    first.run(READY, None).unwrap();
    first.run("DEFINE COLLECTION users;", None).unwrap();

    let mut second = Client::connect(&address).unwrap();
    let refused = second
        .run("SELECT * FROM users;", None)
        .expect_err("no namespace is selected on a fresh connection");
    assert!(!refused.to_string().is_empty());
}

#[test]
fn a_reference_arrives_with_the_name_the_client_could_not_have_looked_up() {
    // The catalog is on the server. A client rendering from ids alone prints
    // `<record 3:7>`, which is the one thing a console promising its output
    // pastes back cannot print.
    let (_node, address) = serving(Db::in_memory().unwrap());
    let mut client = Client::connect(&address).unwrap();
    client
        .run(
            &format!("{READY} DEFINE COLLECTION users; DEFINE COLLECTION orders;"),
            None,
        )
        .unwrap();
    let answers = client
        .run(
            "CREATE orders:1 = { by: users:7 }; SELECT * FROM orders;",
            None,
        )
        .unwrap();

    let Answer::Records { records, names, .. } = &answers[1] else {
        panic!("not records: {:?}", answers[1]);
    };
    let Value::Object(held) = &records[0].1 else {
        panic!("not an object: {:?}", records[0].1);
    };
    let Some(Value::Record(reference)) = held.get("by") else {
        panic!("not a reference: {held:?}");
    };
    assert_eq!(
        names.get(&reference.table).map(String::as_str),
        Some("users")
    );
}

#[test]
fn a_refusal_arrives_as_a_refusal_and_the_connection_lives() {
    // A client that mistyped a statement has not stopped being a client.
    let (_node, address) = serving(Db::in_memory().unwrap());
    let mut client = Client::connect(&address).unwrap();

    let refused = client.run("SELECT * FROM;", None).expect_err("a refusal");
    let message = refused.to_string();
    assert!(message.contains("expected"), "{message}");

    // And the next statement runs on the same connection.
    let answers = client.run(READY, None).unwrap();
    assert_eq!(answers.len(), 4);
}

#[test]
fn a_closed_store_refuses_an_unauthenticated_request_with_the_sessions_own_words() {
    // The check is the session's. A second rule here would be a second place for
    // "who may do this" to be decided, and the two would eventually disagree.
    let db = Db::in_memory().unwrap();
    {
        let mut session = db.session();
        session
            .run_now("DEFINE USER root ROLE owner PASSWORD 'a long one';")
            .unwrap();
    }
    let (_node, address) = serving(db);
    let mut client = Client::connect(&address).unwrap();

    let refused = client
        .run("DEFINE NAMESPACE prod;", None)
        .expect_err("a refusal");
    assert!(refused.to_string().contains("signed-in"), "{refused}");

    // With credentials it runs.
    let answers = client
        .run("DEFINE NAMESPACE prod;", Some(("root", "a long one")))
        .unwrap();
    assert_eq!(answers.len(), 1);

    // And a wrong password is the session's refusal, not a different one.
    let refused = client
        .run("DEFINE NAMESPACE other;", Some(("root", "wrong")))
        .expect_err("a refusal");
    assert!(refused.to_string().contains("no user"), "{refused}");
}

#[test]
fn a_frame_claiming_more_than_the_node_will_read_does_not_take_the_node_down() {
    // The oldest denial of service there is: a five-byte header claiming a
    // gibibyte. It is refused before the allocation, the connection closes, and
    // the node keeps serving — asserted by a second client arriving afterwards.
    let (_node, address) = serving(Db::in_memory().unwrap());
    {
        let mut hostile = TcpStream::connect(&address).unwrap();
        // The greeting, so the node gets as far as reading a frame.
        hostile.write_all(b"TESS").unwrap();
        hostile.write_all(&[1]).unwrap();
        hostile.write_all(&[1]).unwrap();
        hostile.write_all(&1_000_000_000_u32.to_be_bytes()).unwrap();
        hostile.flush().unwrap();
    }

    let mut client = Client::connect(&address).unwrap();
    assert_eq!(client.run(READY, None).unwrap().len(), 4);
}

#[test]
fn rubbish_on_the_socket_is_refused_at_the_greeting() {
    let (_node, address) = serving(Db::in_memory().unwrap());
    {
        let mut hostile = TcpStream::connect(&address).unwrap();
        hostile.write_all(b"GET / HTTP/1.1\r\n\r\n").unwrap();
        hostile.flush().unwrap();
    }
    // The node survives it.
    let mut client = Client::connect(&address).unwrap();
    assert_eq!(client.run(READY, None).unwrap().len(), 4);
}

#[test]
fn talking_to_something_that_is_not_a_node_says_so_rather_than_hanging() {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap().to_string();
    drop(std::thread::spawn(move || {
        if let Ok((mut stream, _)) = listener.accept() {
            drop(stream.write_all(b"HTTP/1.1 200 OK\r\n\r\n"));
        }
    }));

    let refused = Client::connect(&address).expect_err("a refusal");
    assert!(
        refused.to_string().contains("not a TessariDB node"),
        "{refused}"
    );
}

#[test]
fn two_clients_at_once_do_not_interfere() {
    let (_node, address) = serving(Db::in_memory().unwrap());
    {
        let mut setup = Client::connect(&address).unwrap();
        setup
            .run(&format!("{READY} DEFINE COLLECTION counters;"), None)
            .unwrap();
    }

    let handles: Vec<_> = (0..4)
        .map(|worker| {
            let address = address.clone();
            std::thread::spawn(move || {
                let mut client = Client::connect(&address).unwrap();
                for n in 0..10 {
                    let id = worker * 100 + n;
                    client
                        .run(
                            &format!(
                                "USE NAMESPACE prod; USE DATABASE orders; \
                                 CREATE counters:{id} = {{ worker: {worker} }};"
                            ),
                            None,
                        )
                        .unwrap();
                }
            })
        })
        .collect();
    for handle in handles {
        handle.join().unwrap();
    }

    let mut client = Client::connect(&address).unwrap();
    let answers = client
        .run(
            "USE NAMESPACE prod; USE DATABASE orders; \
             SELECT count(*) AS held FROM counters;",
            None,
        )
        .unwrap();
    let Answer::Records { records, .. } = &answers[2] else {
        panic!("not records");
    };
    let Value::Object(held) = &records[0].1 else {
        panic!("not an object");
    };
    assert_eq!(
        held.get("held"),
        Some(&Value::Number(tessaridb::Number::from(40_i64)))
    );
}

#[test]
fn a_bounded_read_this_node_cannot_answer_is_redirected_over_the_wire() {
    // S6.2's routing half, along the whole chain rather than at either end of
    // it: a real `Directory`, published the way the dialling thread publishes
    // one, handed to a real node, asked by a real client over a real socket.
    //
    // The unit tests either side of this one assert that the directory picks
    // the right peer and that the session uses the answer. Neither of them can
    // catch a node that was given the directory and never passed it to the
    // sessions it opens, which is the one thing this test is here for.
    let mut directory = tessari_wire::Directory::new();
    directory.heard(
        "two.example:9080",
        tessari_wire::Hello {
            node: [3; tessari_encoding::NODE_ID_LEN],
            build: tessari_encoding::NodeVersion {
                major: 0,
                minor: 1,
                patch: 1,
            },
            epoch: tessari_types::Epoch::new(1),
            roles: tessari_encoding::Roles::SERVING,
            tail: tessari_types::Sequence::new(4096),
            tail_leadership: tessari_types::Epoch::new(1),
            current_as_of: Some(std::time::Duration::from_secs(1)),
            policy: None,
            line: None,
        },
        std::time::Instant::now(),
    );

    let db = Db::in_memory().unwrap();
    {
        // The schema first and the role second, in that order: a node that may
        // not write cannot define a collection either, so the two lines are not
        // interchangeable. A node that may not write holds somebody else's
        // writes, and this build cannot say how old they are — which is what
        // puts its own copy outside every bound and sends the read elsewhere.
        let mut session = db.session();
        session
            .run_now(&format!("{READY} DEFINE COLLECTION users;"))
            .unwrap();
        session.run_now("DEFINE NODE ROLES serving;").unwrap();
    }
    let (_node, address) = serving_among(db, Arc::new(tessari_wire::Published::holding(directory)));
    let mut client = Client::connect(&address).unwrap();

    // **Not `expect_err`.** Until this wave the redirect arrived as a refusal
    // and this block read the address out of prose; a client had to parse an
    // error message to route, which is the failure `redirect.rs` names.
    let served = client
        .run_routed(
            "USE NAMESPACE prod; USE DATABASE orders; SELECT * FROM users STALENESS 60s;",
            None,
            &std::collections::BTreeMap::new(),
        )
        .expect("a redirect is an instruction, and an instruction is not a failure");
    let tessari_wire::Served::Elsewhere(sent) = served else {
        panic!("the node answered something that was not a redirect: {served:?}");
    };
    assert_eq!(sent.endpoint, "two.example:9080");
    assert_eq!(
        sent.node,
        [3; tessari_encoding::NODE_ID_LEN],
        "a redirect naming only a place cannot be checked on arrival"
    );
    // The epoch is the one **that** node published in its greeting, not this
    // node's own: a node redirecting a bounded read is one whose copy failed the
    // bound, and it may hold no leadership at all.
    assert_eq!(
        sent.epoch,
        tessari_types::Epoch::new(1),
        "the redirect crossed the wire undated, so nothing downstream can tell a          loop from progress"
    );
    assert_eq!(
        sent.settlement,
        tessari_wire::Settlement::Transient,
        "a staleness redirect is about this read; a client that remembered it          would pin its map to a freshness accident"
    );

    // The other contract, unchanged and still honoured: `run` promises the
    // answers, so it cannot hand back a redirect — but it names where the read
    // belonged instead of reporting tag 13 as an unknown frame.
    let refused = client
        .run(
            "USE NAMESPACE prod; USE DATABASE orders; SELECT * FROM users STALENESS 60s;",
            None,
        )
        .expect_err("`run` asked for answers and there are none here");
    assert!(
        refused.to_string().contains("two.example:9080"),
        "the caller that cannot follow a redirect was not told where the read          belonged: {refused}"
    );

    // And the connection lives, because a redirect is an answer.
    let answers = client
        .run(
            "USE NAMESPACE prod; USE DATABASE orders; SELECT * FROM users;",
            None,
        )
        .unwrap();
    assert_eq!(answers.len(), 3);
}

/// Serve `node` on a runtime of this test's own: the node creates none.
fn serve_until_the_test_ends(node: &Node) {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap();
    drop(runtime.block_on(node.serve(tokio_util::sync::CancellationToken::new())));
}

/// A certificate for `127.0.0.1` issued by a fresh authority: the node's PEM
/// chain and key, and the authority's PEM a client trusts it by.
fn issued_for_loopback() -> (String, String, String) {
    let authority_key = rcgen::KeyPair::generate().unwrap();
    let mut asked = rcgen::CertificateParams::new(Vec::<String>::new()).unwrap();
    asked.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
    let authority = asked.self_signed(&authority_key).unwrap();
    let leaf_key = rcgen::KeyPair::generate().unwrap();
    let leaf = rcgen::CertificateParams::new(vec!["127.0.0.1".to_owned()])
        .unwrap()
        .signed_by(&leaf_key, &authority, &authority_key)
        .unwrap();
    (leaf.pem(), leaf_key.serialize_pem(), authority.pem())
}

fn trusting(pem: &str) -> rustls::RootCertStore {
    tessari_serve::tls::authority(tessari_serve::tls::Pem {
        bytes: pem.as_bytes(),
        path: "ca.pem",
    })
    .unwrap()
}

/// A node that speaks TLS and nothing else, and the authority that issued it.
fn serving_tls() -> (Arc<Node>, String, String) {
    let (chain, key, authority) = issued_for_loopback();
    let settings = tessari_serve::tls::Credential::read(
        tessari_serve::tls::Pem {
            bytes: chain.as_bytes(),
            path: "cert.pem",
        },
        tessari_serve::tls::Pem {
            bytes: key.as_bytes(),
            path: "key.pem",
        },
    )
    .unwrap()
    .server_config(&[]);
    let (node, address) = started(
        Node::bind(Arc::new(Db::in_memory().unwrap()), "127.0.0.1:0")
            .unwrap()
            .securing(settings),
    );
    (node, address, authority)
}

#[test]
fn a_node_with_a_certificate_answers_a_client_that_verified_it() {
    let (_node, address, authority) = serving_tls();
    let mut client = Client::connect_tls(&address, trusting(&authority)).unwrap();
    let answers = client.run("RETURN 40 + 2;", None).unwrap();
    assert_eq!(answers.len(), 1);
    assert!(
        matches!(&answers[0], Answer::Value { value, .. } if *value == Value::Number(tessaridb::Number::Integer(42))),
        "{answers:?}"
    );
}

#[test]
fn a_node_with_a_certificate_never_answers_the_protocol_in_the_clear() {
    let (_node, address, _) = serving_tls();
    let refused = Client::connect(&address).expect_err("a plaintext greeting was answered");
    // What comes back is a TLS alert, which a plaintext client reads as
    // something that is not a node — or nothing, if the node hung up first.
    assert!(
        matches!(
            refused,
            tessari_wire::Error::NotThisProtocol | tessari_wire::Error::Io(_)
        ),
        "{refused}"
    );
    // And the bytes that came back, if any, are not the protocol's magic.
    let mut raw = TcpStream::connect(&address).unwrap();
    raw.write_all(b"TESS\x01\x02").unwrap();
    let mut back = Vec::new();
    drop(std::io::Read::read_to_end(&mut raw, &mut back));
    assert!(!back.starts_with(b"TESS"), "the node greeted in the clear");
}

#[test]
fn a_client_trusting_another_authority_is_refused_by_its_own_check() {
    let (_node, address, _) = serving_tls();
    let (_, _, someone_elses) = issued_for_loopback();
    let refused = Client::connect_tls(&address, trusting(&someone_elses))
        .expect_err("a certificate from an authority the client does not trust");
    assert!(matches!(refused, tessari_wire::Error::Tls(_)), "{refused}");
}

#[test]
fn a_node_with_a_certificate_refuses_a_client_that_offers_only_tls_1_2() {
    let (_node, address, authority) = serving_tls();
    let settings = rustls::ClientConfig::builder_with_protocol_versions(&[&rustls::version::TLS12])
        .with_root_certificates(trusting(&authority))
        .with_no_client_auth();
    let name = rustls::pki_types::ServerName::try_from("127.0.0.1").unwrap();
    let mut session = rustls::ClientConnection::new(Arc::new(settings), name).unwrap();
    let mut socket = TcpStream::connect(&address).unwrap();
    let mut refused = None;
    while session.is_handshaking() {
        if let Err(why) = session.complete_io(&mut socket) {
            refused = Some(why);
            break;
        }
    }
    let refused = refused.expect("a TLS 1.2 handshake completed");
    assert!(
        refused.to_string().contains("ProtocolVersion")
            || refused.to_string().contains("protocol version"),
        "{refused}"
    );
}

#[test]
fn a_missing_record_and_an_event_that_met_one_arrive_as_conflicts() {
    // ADR-0124 D7: the class follows the cause, over the wire as in the session.
    let (_node, address) = serving(Db::in_memory().unwrap());
    let mut client = Client::connect(&address).unwrap();
    client
        .run(
            &format!(
                "{READY} DEFINE COLLECTION notes; DEFINE COLLECTION other; \
                 DEFINE EVENT touch ON notes FOR CREATE THEN UPDATE other:1 MERGE {{ x: 1 }};"
            ),
            None,
        )
        .unwrap();
    for statement in [
        "UPDATE other:404 MERGE { x: 1 };",
        "CREATE notes:1 = { body: 'a' };",
    ] {
        match client.run(statement, None) {
            Err(tessari_wire::Error::Refused { class, message }) => assert_eq!(
                class,
                Some(Some(tessari_types::RefusalClass::Conflict)),
                "{statement}: {message}"
            ),
            other => panic!("{statement}: {other:?}"),
        }
    }
}

#[test]
fn an_answer_above_the_frame_ceiling_is_refused_by_name_and_the_connection_lives() {
    // A 16 MiB frame cannot carry it, and the node used to fail the write and
    // drop the connection, which a client reads as a network fault. It is a
    // refusal instead: the request was fine to send and cannot be answered so.
    let (_node, address) = serving(above_the_ceiling());
    let mut client = Client::connect(&address).unwrap();
    client
        .run("USE NAMESPACE prod; USE DATABASE orders;", None)
        .unwrap();

    match client.run("SELECT * FROM big;", None) {
        Err(tessari_wire::Error::Refused { class, message }) => {
            assert_eq!(
                class,
                Some(Some(tessari_types::RefusalClass::Invalid)),
                "{message}"
            );
            assert!(message.contains("16777216"), "{message}");
            assert!(message.contains("BACKUP"), "{message}");
        }
        other => panic!("expected a refusal, got {other:?}"),
    }
    // And the same connection answers the next request.
    let answers = client.run("SELECT id FROM big;", None).unwrap();
    assert_eq!(answers.len(), 1);
}

/// A store holding two 9 MB records, so a read of both, or its snapshot, is
/// more than one 16 MiB frame carries.
fn above_the_ceiling() -> Db {
    let db = Db::in_memory().unwrap();
    {
        let mut session = db.session();
        session
            .run_now(&format!("{READY} DEFINE COLLECTION big;"))
            .unwrap();
        let nine_megabytes = Value::String("x".repeat(9 * 1024 * 1024));
        let body = std::collections::BTreeMap::from([("body".to_owned(), nine_megabytes)]);
        for statement in [
            "CREATE big:1 = { body: $body };",
            "CREATE big:2 = { body: $body };",
        ] {
            session.run_with_now(statement, &body).unwrap();
        }
    }
    db
}

#[test]
fn a_snapshot_above_the_ceiling_streams_in_parts_and_arrives_whole() {
    // ADR-0125: asked for, the snapshot arrives as parts ahead of the answer,
    // and the answer is the summary, since the bytes have already left.
    let (_node, address) = serving(above_the_ceiling());
    let mut client = Client::connect(&address).unwrap();
    let mut sink = Vec::new();
    let answers = client
        .run_streaming(
            "BACKUP STATE;",
            None,
            &std::collections::BTreeMap::new(),
            &mut sink,
        )
        .unwrap();

    assert!(sink.len() > 16 * 1024 * 1024, "{} bytes", sink.len());
    let verified = tessari_backup::verify_state(&mut sink.as_slice()).unwrap();
    let [
        Answer::Value {
            value: Value::Object(summary),
            ..
        },
    ] = answers.as_slice()
    else {
        panic!("one summary, got {answers:?}");
    };
    assert_eq!(summary.get("form"), Some(&Value::from("state")));
    assert_eq!(
        summary.get("records"),
        Some(&Value::from(i64::try_from(verified.records).unwrap()))
    );

    // The connection goes on, and a request that takes no snapshot streams
    // nothing — including one sent after the streamed one, so no sink is left
    // behind for it.
    let mut nothing = Vec::new();
    let answers = client
        .run_streaming(
            "USE NAMESPACE prod; USE DATABASE orders; SELECT id FROM big;",
            None,
            &std::collections::BTreeMap::new(),
            &mut nothing,
        )
        .unwrap();
    assert_eq!(answers.len(), 3);
    assert!(nothing.is_empty(), "{} bytes", nothing.len());
}

#[test]
fn a_snapshot_not_asked_to_stream_is_still_refused_above_the_ceiling() {
    // Without the flag nothing changes: the snapshot is the statement's value,
    // and above one frame it is the named refusal.
    let (_node, address) = serving(above_the_ceiling());
    let mut client = Client::connect(&address).unwrap();
    match client.run("BACKUP STATE;", None) {
        Err(tessari_wire::Error::Refused { class, message }) => {
            assert_eq!(
                class,
                Some(Some(tessari_types::RefusalClass::Invalid)),
                "{message}"
            );
        }
        other => panic!("expected a refusal, got {other:?}"),
    }
}

#[test]
fn a_stream_the_script_never_took_does_not_catch_the_next_snapshot() {
    // A request that asked to stream and took no snapshot leaves nothing set on
    // the connection: the next plain BACKUP answers with its bytes as before.
    //
    // Three requests back to back, because the node keeps one session attached
    // across requests that arrive close behind each other (`hot.rs`), and that is
    // the session a sink left behind would still be set on.
    let (_node, address) = serving(Db::in_memory().unwrap());
    let mut client = Client::connect(&address).unwrap();
    client.run(READY, None).unwrap();
    let mut nothing = Vec::new();
    client
        .run_streaming(
            "USE NAMESPACE prod;",
            None,
            &std::collections::BTreeMap::new(),
            &mut nothing,
        )
        .unwrap();
    assert!(nothing.is_empty());

    let answers = client.run("BACKUP STATE;", None).unwrap();
    let [Answer::Value { value, .. }] = answers.as_slice() else {
        panic!("one value, got {answers:?}");
    };
    let Value::Bytes(bytes) = value else {
        panic!("the snapshot's bytes, got {value:?}");
    };
    tessari_backup::verify_state(&mut bytes.as_slice()).unwrap();
}
