//! `GET /wire` — the wire protocol carried over a WebSocket (ADR-0089).
//!
//! The claim under test is that the socket carries the wire protocol *unchanged*,
//! so the strongest test is a comparison: the same scripts sent to one node over
//! TCP and to an identical node over `/wire` must come back as the same bytes.
//! The WebSocket side is written out by hand, masked, as a browser sends it —
//! nothing here asks the code under test how a frame should look.

#![allow(clippy::panic, clippy::unwrap_used, clippy::indexing_slicing)]

use std::future::Future;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::pin::Pin;
use std::sync::Arc;

use tessari_http::{Node, WireDoor, WireSession};
use tessari_wire::{Follow, Request};
use tessaridb::Db;

/// The line between a failing assertion and a hung suite.
const PATIENCE: std::time::Duration = std::time::Duration::from_secs(5);

const KEY: &str = "dGhlIHNhbXBsZSBub25jZQ==";

/// Frame kinds, as the specification numbers them.
const REQUEST: u8 = 1;
const ANSWER: u8 = 2;
const REFUSAL: u8 = 3;
const SUBSCRIBE: u8 = 4;
const CHANGE: u8 = 5;

/// The wire node's carrier, in the shape the HTTP node takes it.
pub(crate) fn door_of(wire: &tessari_wire::Node) -> WireDoor {
    let carrier = wire.carrier();
    Arc::new(move || {
        carrier.admit().map(|admitted| {
            let session: WireSession =
                Box::new(move |stream| -> Pin<Box<dyn Future<Output = ()> + Send>> {
                    Box::pin(admitted.converse(stream))
                });
            session
        })
    })
}

/// One store served on both surfaces, the HTTP one carrying the wire.
struct Served {
    wire: String,
    http: String,
}

fn served() -> Served {
    let db = Arc::new(Db::in_memory().unwrap());
    let wire = Arc::new(tessari_wire::Node::bind(Arc::clone(&db), "127.0.0.1:0").unwrap());
    let mut http = Node::bind(Arc::clone(&db), "127.0.0.1:0").unwrap();
    http.carrying(door_of(&wire));
    let http = Arc::new(http);
    let addresses = Served {
        wire: wire.address().unwrap(),
        http: http.address(),
    };
    std::thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap();
        drop(runtime.block_on(wire.serve(tokio_util::sync::CancellationToken::new())));
    });
    std::thread::spawn(move || crate::serve_until_the_test_ends(&http));
    addresses
}

/// An HTTP node alone, carrying the door the test decides — or none.
fn served_with(door: Option<WireDoor>) -> String {
    let db = Arc::new(Db::in_memory().unwrap());
    let mut http = Node::bind(db, "127.0.0.1:0").unwrap();
    if let Some(door) = door {
        http.carrying(door);
    }
    let http = Arc::new(http);
    let address = http.address();
    std::thread::spawn(move || crate::serve_until_the_test_ends(&http));
    address
}

// ---- the wire, spoken by hand -------------------------------------------------

fn frame(kind: u8, body: &[u8]) -> Vec<u8> {
    let mut out = vec![kind];
    out.extend_from_slice(&u32::try_from(body.len()).unwrap().to_be_bytes());
    out.extend_from_slice(body);
    out
}

const GREETING: [u8; 6] = [b'T', b'E', b'S', b'S', 1, 4];

fn request(script: &str) -> Vec<u8> {
    frame(
        REQUEST,
        &Request {
            script: script.to_owned(),
            credentials: None,
            parameters: tessaridb::Parameters::new(),
        }
        .encode(),
    )
}

/// Read one wire frame from any byte source.
fn wire_frame(bytes: &mut impl FnMut(usize) -> Vec<u8>) -> (u8, Vec<u8>) {
    let header = bytes(5);
    let length = u32::from_be_bytes([header[1], header[2], header[3], header[4]]);
    (header[0], bytes(usize::try_from(length).unwrap()))
}

fn reader(stream: &mut TcpStream) -> impl FnMut(usize) -> Vec<u8> + '_ {
    move |wanted| {
        let mut bytes = vec![0u8; wanted];
        stream.read_exact(&mut bytes).unwrap();
        bytes
    }
}

fn tcp(address: &str) -> TcpStream {
    let mut stream = TcpStream::connect(address).unwrap();
    stream.set_read_timeout(Some(PATIENCE)).unwrap();
    stream.write_all(&GREETING).unwrap();
    let mut theirs = [0u8; 6];
    stream.read_exact(&mut theirs).unwrap();
    assert_eq!(
        theirs, GREETING,
        "the TCP greeting is not the wire greeting"
    );
    stream
}

// ---- the WebSocket, spoken by hand --------------------------------------------

fn upgrade(address: &str, extra: &str) -> (TcpStream, u16) {
    let mut stream = TcpStream::connect(address).unwrap();
    stream.set_read_timeout(Some(PATIENCE)).unwrap();
    let request = format!(
        "GET /wire HTTP/1.1\r\nHost: {address}\r\nUpgrade: websocket\r\n\
         Connection: Upgrade\r\nSec-WebSocket-Key: {KEY}\r\n\
         Sec-WebSocket-Version: 13\r\n{extra}\r\n"
    );
    stream.write_all(request.as_bytes()).unwrap();
    let mut raw = Vec::new();
    let mut byte = [0u8; 1];
    while !raw.ends_with(b"\r\n\r\n") {
        if stream.read_exact(&mut byte).is_err() {
            break;
        }
        raw.push(byte[0]);
    }
    let text = String::from_utf8_lossy(&raw).into_owned();
    let status = text
        .split_whitespace()
        .nth(1)
        .and_then(|code| code.parse().ok())
        .unwrap_or(0);
    (stream, status)
}

/// Send one WebSocket message, masked as a client must.
fn send(stream: &mut TcpStream, opcode: u8, payload: &[u8]) {
    let mask = [0x37u8, 0xfa, 0x21, 0x3d];
    let mut out = vec![0b1000_0000 | opcode];
    match payload.len() {
        length if length < 126 => out.push(0b1000_0000 | u8::try_from(length).unwrap()),
        length if length <= usize::from(u16::MAX) => {
            out.push(0b1000_0000 | 126);
            out.extend_from_slice(&u16::try_from(length).unwrap().to_be_bytes());
        }
        length => {
            out.push(0b1000_0000 | 127);
            out.extend_from_slice(&u64::try_from(length).unwrap().to_be_bytes());
        }
    }
    out.extend_from_slice(&mask);
    out.extend(
        payload
            .iter()
            .enumerate()
            .map(|(index, byte)| byte ^ mask[index % 4]),
    );
    stream.write_all(&out).unwrap();
}

/// Read one WebSocket message: its opcode and payload.
fn message(stream: &mut TcpStream) -> (u8, Vec<u8>) {
    let mut header = [0u8; 2];
    stream.read_exact(&mut header).unwrap();
    assert_eq!(
        header[0] & 0b1000_0000,
        0b1000_0000,
        "no test here expects a fragment"
    );
    let length = match header[1] & 0b0111_1111 {
        126 => {
            let mut wide = [0u8; 2];
            stream.read_exact(&mut wide).unwrap();
            usize::from(u16::from_be_bytes(wide))
        }
        127 => {
            let mut wide = [0u8; 8];
            stream.read_exact(&mut wide).unwrap();
            usize::try_from(u64::from_be_bytes(wide)).unwrap()
        }
        short => usize::from(short),
    };
    let mut payload = vec![0u8; length];
    stream.read_exact(&mut payload).unwrap();
    (header[0] & 0b0000_1111, payload)
}

/// The bytes a `/wire` socket carries, reassembled across however many binary
/// messages the node chose to send them in.
struct Pipe {
    stream: TcpStream,
    held: Vec<u8>,
}

impl Pipe {
    fn take(&mut self, wanted: usize) -> Vec<u8> {
        while self.held.len() < wanted {
            let (opcode, payload) = message(&mut self.stream);
            assert_eq!(
                opcode, 2,
                "the node sent something that is not a binary message"
            );
            self.held.extend_from_slice(&payload);
        }
        self.held.drain(..wanted).collect()
    }
}

fn open_pipe(address: &str, extra: &str) -> Pipe {
    let (mut stream, status) = upgrade(address, extra);
    assert_eq!(status, 101, "the upgrade to /wire was not accepted");
    send(&mut stream, 2, &GREETING);
    let mut pipe = Pipe {
        stream,
        held: Vec::new(),
    };
    assert_eq!(
        pipe.take(6),
        GREETING,
        "the node's greeting is not the wire greeting"
    );
    pipe
}

/// Every kind of value the wire carries and JSON does not, a session's `USE`
/// carried from one statement to the next, a transaction, and a refusal.
const SCRIPTS: &[&str] = &[
    "DEFINE NAMESPACE app;",
    "USE NAMESPACE app;",
    "DEFINE DATABASE main; USE DATABASE main;",
    "DEFINE COLLECTION things; DEFINE COLLECTION users;",
    "CREATE things:1 = { at: datetime '2026-09-30T10:00:00Z', price: dec 12.34, \
     blob: 0x0a1b, wait: 1h30m, token: uuid '0192e2a8-0000-7000-8000-000000000001', \
     span: 1..10, tags: set ['a', 'b'], owner: users:'ada', \
     place: geometry { type: 'Point', coordinates: [2.35, 48.85] }, gone: NULL };",
    "BEGIN; CREATE things:2 = { n: 2 }; CREATE things:3 = { n: 3 }; COMMIT;",
    "SELECT * FROM things ORDER BY id;",
    "SELECT FROM nowhere;",
    "SELECT count(*) AS n FROM things;",
];

#[test]
fn the_same_scripts_answer_the_same_bytes_over_tcp_and_over_the_socket() {
    let over_tcp = served();
    let over_socket = served();

    let mut stream = tcp(&over_tcp.wire);
    let mut pipe = open_pipe(&over_socket.http, "");
    let mut refusals = 0;

    for script in SCRIPTS {
        stream.write_all(&request(script)).unwrap();
        let tcp_answer = wire_frame(&mut reader(&mut stream));
        send(&mut pipe.stream, 2, &request(script));
        let socket_answer = wire_frame(&mut |wanted| pipe.take(wanted));
        assert!(
            tcp_answer.0 == ANSWER || tcp_answer.0 == REFUSAL,
            "{script}: TCP answered frame kind {}",
            tcp_answer.0
        );
        if tcp_answer.0 == REFUSAL {
            refusals += 1;
        }
        assert_eq!(
            socket_answer, tcp_answer,
            "{script}: the socket answered different bytes from TCP"
        );
    }
    assert_eq!(
        refusals, 1,
        "exactly one script is meant to be refused, so that a refusal is compared too"
    );
}

#[test]
fn a_frame_split_across_messages_and_frames_joined_in_one_message_are_both_answered() {
    let node = served();
    let mut pipe = open_pipe(&node.http, "");

    // One frame in three messages, cut inside the header and inside the body.
    let one = request("RETURN 7;");
    send(&mut pipe.stream, 2, &one[..3]);
    send(&mut pipe.stream, 2, &one[3..9]);
    send(&mut pipe.stream, 2, &one[9..]);
    let (kind, _) = wire_frame(&mut |wanted| pipe.take(wanted));
    assert_eq!(
        kind, ANSWER,
        "a frame split across messages was not answered"
    );

    // Three frames in one message.
    let mut three = request("RETURN 1;");
    three.extend(request("RETURN 2;"));
    three.extend(request("RETURN 3;"));
    send(&mut pipe.stream, 2, &three);
    for _ in 0..3 {
        let (kind, _) = wire_frame(&mut |wanted| pipe.take(wanted));
        assert_eq!(
            kind, ANSWER,
            "a frame joined to others in one message was not answered"
        );
    }
}

#[test]
fn a_subscription_over_the_socket_delivers_a_change() {
    let node = served();
    let mut writer = tcp(&node.wire);
    writer
        .write_all(&request(
            "DEFINE NAMESPACE app; USE NAMESPACE app; DEFINE DATABASE main; \
             USE DATABASE main; DEFINE COLLECTION notes;",
        ))
        .unwrap();
    assert_eq!(wire_frame(&mut reader(&mut writer)).0, ANSWER);

    let mut pipe = open_pipe(&node.http, "");
    send(
        &mut pipe.stream,
        2,
        &request("USE NAMESPACE app; USE DATABASE main;"),
    );
    assert_eq!(wire_frame(&mut |wanted| pipe.take(wanted)).0, ANSWER);
    send(
        &mut pipe.stream,
        2,
        &frame(
            SUBSCRIBE,
            &Follow {
                from: 0,
                table: Some("notes".to_owned()),
                cursor: None,
                condition: None,
            }
            .encode(),
        ),
    );

    writer
        .write_all(&request("CREATE notes:1 = { text: 'hello' };"))
        .unwrap();
    assert_eq!(wire_frame(&mut reader(&mut writer)).0, ANSWER);

    // From the start of the log, so whatever else the log holds for this table
    // comes first; the write above is the only one carrying this text.
    let delivered = (0..8).any(|_| {
        let (kind, body) = wire_frame(&mut |wanted| pipe.take(wanted));
        assert_eq!(
            kind, CHANGE,
            "the subscription answered something that is not a change"
        );
        body.windows(5).any(|window| window == b"hello")
    });
    assert!(
        delivered,
        "the subscription over the socket never delivered the record that was written"
    );
}

#[test]
fn an_authorization_header_on_the_upgrade_signs_nobody_in() {
    let node = served();
    let mut owner = tcp(&node.wire);
    owner
        .write_all(&request(
            "DEFINE USER root ROLE owner PASSWORD 'a long owner password';",
        ))
        .unwrap();
    assert_eq!(
        wire_frame(&mut reader(&mut owner)).0,
        ANSWER,
        "the owner was not declared"
    );

    // A browser attaches a cached Basic credential to any handshake, from any
    // page; honouring it would let that page act as the user.
    let mut pipe = open_pipe(
        &node.http,
        "Authorization: Basic cm9vdDphIGxvbmcgb3duZXIgcGFzc3dvcmQ=\r\n",
    );
    send(&mut pipe.stream, 2, &request("INFO FOR STORE;"));
    let (kind, _) = wire_frame(&mut |wanted| pipe.take(wanted));
    assert_eq!(
        kind, REFUSAL,
        "the socket ran a statement on a closed store with no credential in the \
         request, so the handshake's header signed somebody in"
    );
}

#[test]
fn a_text_message_closes_the_socket_with_1003() {
    let node = served();
    let mut pipe = open_pipe(&node.http, "");
    send(&mut pipe.stream, 1, b"SELECT 1;");
    let (opcode, payload) = message(&mut pipe.stream);
    assert_eq!(opcode, 8, "a text message was not answered with a close");
    assert_eq!(
        u16::from_be_bytes([payload[0], payload[1]]),
        1003,
        "the close does not say the data was of a kind this route does not take"
    );
}

#[test]
fn a_node_without_the_wire_answers_404_and_a_full_door_503() {
    let without = served_with(None);
    let (_, status) = upgrade(&without, "");
    assert_eq!(
        status, 404,
        "a node not serving the wire protocol upgraded /wire"
    );

    let full: WireDoor = Arc::new(|| None);
    let full = served_with(Some(full));
    let (_, status) = upgrade(&full, "");
    assert_eq!(status, 503, "a full door upgraded anyway");
}
