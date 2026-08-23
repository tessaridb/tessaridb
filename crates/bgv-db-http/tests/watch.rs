//! What a browser sees when it opens the socket route.
//!
//! Over a real TCP connection, with the client side written out by hand — masked
//! frames, a key, and the accept value **quoted from RFC 6455 §1.3** rather than
//! computed by the code under test. That last part is the point: a test that
//! asks this crate what the answer should be and then checks it got that answer
//! is two copies of one opinion, and it passes whatever the implementation does.

#![allow(clippy::panic, clippy::unwrap_used, clippy::indexing_slicing)]

use std::io::{Read, Write};
use std::net::TcpStream;
use std::sync::Arc;

use bgv_db::Db;
use bgv_db_http::Node;

/// The key and the answer, both printed in RFC 6455 §1.3.
const KEY: &str = "dGhlIHNhbXBsZSBub25jZQ==";
const ACCEPT: &str = "s3pPLMBiTxaQ9kYGzzhZRbK+xOo=";

/// How long a test waits for a frame before deciding none is coming.
///
/// Everything here is loopback and answered on arrival, so this is not a
/// tolerance — it is the line between a failing assertion and a hung suite.
const PATIENCE: std::time::Duration = std::time::Duration::from_secs(5);

/// A node on a loopback port the operating system picked, plus its address.
fn node() -> (Arc<Node>, String) {
    let db = Arc::new(Db::in_memory().unwrap());
    let node = Arc::new(Node::bind(db, "127.0.0.1:0").unwrap());
    let address = node.address();
    let serving = Arc::clone(&node);
    std::thread::spawn(move || serving.serve());
    (node, address)
}

/// Read the response head one byte at a time.
///
/// Deliberately not buffered: a `BufReader` would pull the first frames into its
/// own buffer along with the headers, and those frames are what the rest of the
/// test is about.
fn head(stream: &mut TcpStream) -> (u16, Vec<String>) {
    let mut raw = Vec::new();
    let mut byte = [0u8; 1];
    while !raw.ends_with(b"\r\n\r\n") {
        if stream.read_exact(&mut byte).is_err() {
            break;
        }
        raw.push(byte[0]);
    }
    let text = String::from_utf8_lossy(&raw).into_owned();
    let mut lines = text.lines();
    let status = lines
        .next()
        .unwrap_or_default()
        .split_whitespace()
        .nth(1)
        .unwrap_or("0")
        .parse()
        .unwrap_or(0);
    (status, lines.map(str::to_owned).collect())
}

/// Ask to upgrade, offering everything a browser offers.
fn upgrade(address: &str) -> (TcpStream, u16, Vec<String>) {
    let mut stream = TcpStream::connect(address).unwrap();
    // Without this, a frame the node never sends is not a failing test — it is a
    // test that blocks forever, which in a suite reads as a hang and in CI as a
    // timeout with no message. A falsification proved that the hard way: with the
    // pong suppressed, every socket test stopped reporting anything at all.
    stream.set_read_timeout(Some(PATIENCE)).unwrap();
    let request = format!(
        "GET /watch HTTP/1.1\r\nHost: {address}\r\nUpgrade: websocket\r\n\
         Connection: keep-alive, Upgrade\r\nSec-WebSocket-Key: {KEY}\r\n\
         Sec-WebSocket-Version: 13\r\nSec-WebSocket-Extensions: permessage-deflate\r\n\r\n"
    );
    stream.write_all(request.as_bytes()).unwrap();
    stream.flush().unwrap();
    let (status, headers) = head(&mut stream);
    (stream, status, headers)
}

/// The value of `field` in `headers`, if it is there at all.
fn header<'a>(headers: &'a [String], field: &str) -> Option<&'a str> {
    headers.iter().find_map(|line| {
        let (name, value) = line.split_once(':')?;
        name.trim()
            .eq_ignore_ascii_case(field)
            .then(|| value.trim())
    })
}

/// Send a frame the way a client must: masked.
fn send(stream: &mut TcpStream, fin: bool, opcode: u8, payload: &[u8]) {
    let mask = [0x5au8, 0x0f, 0xc3, 0x91];
    let mut out = vec![if fin { 0b1000_0000 | opcode } else { opcode }];
    out.push(0b1000_0000 | u8::try_from(payload.len()).unwrap());
    out.extend_from_slice(&mask);
    out.extend(
        payload
            .iter()
            .enumerate()
            .map(|(index, byte)| byte ^ mask[index % 4]),
    );
    stream.write_all(&out).unwrap();
    stream.flush().unwrap();
}

/// Read one frame the server sent. Server frames are never masked.
fn receive(stream: &mut TcpStream) -> (bool, u8, Vec<u8>) {
    let mut header = [0u8; 2];
    stream
        .read_exact(&mut header)
        .expect("the node sent no frame at all, so whatever was asked went unanswered");
    assert_eq!(
        header[1] & 0b1000_0000,
        0,
        "the server masked a frame, which every browser refuses"
    );
    let length = usize::from(header[1] & 0b0111_1111);
    assert!(
        length < 126,
        "no test here sends enough to need a long form"
    );
    let mut payload = vec![0u8; length];
    stream.read_exact(&mut payload).unwrap();
    (
        header[0] & 0b1000_0000 != 0,
        header[0] & 0b0000_1111,
        payload,
    )
}

#[test]
fn a_browsers_handshake_is_answered_with_the_value_the_specification_prints() {
    let (_node, address) = node();
    let (_stream, status, headers) = upgrade(&address);

    assert_eq!(status, 101, "the upgrade was not accepted");
    assert_eq!(
        header(&headers, "Sec-WebSocket-Accept"),
        Some(ACCEPT),
        "the accept value is not the one RFC 6455 §1.3 prints for this key, so a \
         browser rejects the connection whatever the status line said"
    );
    assert_eq!(
        header(&headers, "Upgrade").map(str::to_ascii_lowercase),
        Some("websocket".to_owned()),
    );
    assert!(
        header(&headers, "Connection").is_some_and(|value| value.eq_ignore_ascii_case("upgrade")),
        "a 101 without `Connection: upgrade` is not an upgrade"
    );
}

#[test]
fn permessage_deflate_is_declined_by_being_left_out_of_the_answer() {
    let (_node, address) = node();
    let (_stream, status, headers) = upgrade(&address);

    assert_eq!(status, 101);
    assert_eq!(
        header(&headers, "Sec-WebSocket-Extensions"),
        None,
        "the offer was echoed, so the client now compresses every message and \
         this node reads none of them"
    );
}

#[test]
fn a_ping_is_answered_over_a_real_socket() {
    let (_node, address) = node();
    let (mut stream, status, _) = upgrade(&address);
    assert_eq!(status, 101);

    send(&mut stream, true, 9, b"knock");
    let (fin, opcode, payload) = receive(&mut stream);
    assert!(fin, "a pong is never fragmented");
    assert_eq!(opcode, 10, "a ping was not answered with a pong");
    assert_eq!(
        payload, b"knock",
        "the pong must carry the ping's payload, which is what an intermediary \
         checks before deciding the connection is alive"
    );
}

#[test]
fn a_close_is_echoed_before_the_socket_goes() {
    let (_node, address) = node();
    let (mut stream, status, _) = upgrade(&address);
    assert_eq!(status, 101);

    send(&mut stream, true, 8, &1001u16.to_be_bytes());
    let (_, opcode, payload) = receive(&mut stream);
    assert_eq!(opcode, 8, "the close was not echoed");
    assert_eq!(
        u16::from_be_bytes([payload[0], payload[1]]),
        1001,
        "the echo carried a different code, so the browser reports an error \
         where a clean end happened"
    );

    let mut rest = Vec::new();
    stream
        .read_to_end(&mut rest)
        .expect("the node echoed the close and then held the socket open");
    assert!(
        rest.is_empty(),
        "the connection kept talking after the close it had already echoed"
    );
}

#[test]
fn a_fragmented_message_is_reassembled_across_a_ping() {
    let (_node, address) = node();
    let (mut stream, status, _) = upgrade(&address);
    assert_eq!(status, 101);

    // A proxy is entitled to split a message, and a control frame is entitled to
    // arrive between the pieces. A loop written as "read until FIN" passes every
    // other test here and fails this one.
    send(&mut stream, false, 1, b"{\"follow\":");
    send(&mut stream, true, 9, b"alive");
    send(&mut stream, true, 0, b"{\"from\":0}}");

    let (_, opcode, payload) = receive(&mut stream);
    assert_eq!(opcode, 10, "the ping between fragments went unanswered");
    assert_eq!(payload, b"alive");

    // SGK.T3 replaces this with the feed; until then the message is refused out
    // loud rather than dropped in silence.
    let (_, opcode, payload) = receive(&mut stream);
    assert_eq!(opcode, 8, "the reassembled message got no answer at all");
    assert_eq!(u16::from_be_bytes([payload[0], payload[1]]), 1003);
}

#[test]
fn an_unmasked_client_frame_ends_the_connection() {
    let (_node, address) = node();
    let (mut stream, status, _) = upgrade(&address);
    assert_eq!(status, 101);

    stream.write_all(&[0b1000_1001, 2, b'h', b'i']).unwrap();
    stream.flush().unwrap();
    let (_, opcode, payload) = receive(&mut stream);
    assert_eq!(opcode, 8);
    assert_eq!(
        u16::from_be_bytes([payload[0], payload[1]]),
        1002,
        "an unmasked client frame was accepted, which removes the protection \
         masking exists for"
    );
}

#[test]
fn an_upgraded_socket_is_counted_as_a_feed_and_not_as_a_request() {
    // The claim the module and the README both make, and which nothing else here
    // proves. It matters at exactly one moment: a shutdown drains requests and
    // waits for them to reach zero. A socket counted as a request is one that
    // never finishes, so every shutdown would run to its full deadline — the
    // defect the two counts were built to prevent.
    let (node, address) = node();
    let stopping = node.stopping();

    let (mut stream, status, _) = upgrade(&address);
    assert_eq!(status, 101);
    // Ask something and read the answer, so the handshake is demonstrably over
    // rather than merely written — the move happens on the node's own thread.
    send(&mut stream, true, 9, b"counted");
    assert_eq!(receive(&mut stream).1, 10);

    assert_eq!(
        (stopping.requests(), stopping.feeds()),
        (0, 1),
        "an upgraded socket must move to the feed count; left among the requests \
         it is something a drain waits for and never gets"
    );
}

#[test]
fn an_ordinary_request_to_the_route_is_told_what_it_speaks() {
    let (_node, address) = node();
    let mut stream = TcpStream::connect(&address).unwrap();
    stream.set_read_timeout(Some(PATIENCE)).unwrap();
    stream
        .write_all(
            format!("GET /watch HTTP/1.1\r\nHost: {address}\r\nConnection: close\r\n\r\n")
                .as_bytes(),
        )
        .unwrap();
    stream.flush().unwrap();
    let (status, _) = head(&mut stream);
    assert_eq!(
        status, 426,
        "a plain GET should be told the route needs an upgrade, not handed a 404 \
         or a page"
    );
}

#[test]
fn the_route_takes_no_other_method() {
    let (_node, address) = node();
    let mut stream = TcpStream::connect(&address).unwrap();
    stream.set_read_timeout(Some(PATIENCE)).unwrap();
    stream
        .write_all(
            format!(
                "POST /watch HTTP/1.1\r\nHost: {address}\r\nContent-Length: 0\r\n\
                 Connection: close\r\n\r\n"
            )
            .as_bytes(),
        )
        .unwrap();
    stream.flush().unwrap();
    let (status, _) = head(&mut stream);
    assert_eq!(
        status, 405,
        "`no such thing` and `not that way` are different answers and a client \
         debugging itself needs to know which one it got"
    );
}
