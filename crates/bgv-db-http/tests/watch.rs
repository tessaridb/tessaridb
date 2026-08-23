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
    upgrade_as(address, None)
}

/// Ask to upgrade while presenting a credential in the request head.
///
/// The arm a browser cannot use: `WebSocket` gives JavaScript no way to set a
/// header, which is why the follow message carries credentials too. Both arms
/// end at the same `sign_in`, so what needs its own test is the *selection*.
fn upgrade_as(address: &str, credential: Option<&str>) -> (TcpStream, u16, Vec<String>) {
    let mut stream = TcpStream::connect(address).unwrap();
    // Without this, a frame the node never sends is not a failing test — it is a
    // test that blocks forever, which in a suite reads as a hang and in CI as a
    // timeout with no message. A falsification proved that the hard way: with the
    // pong suppressed, every socket test stopped reporting anything at all.
    stream.set_read_timeout(Some(PATIENCE)).unwrap();
    let offered =
        credential.map_or_else(String::new, |value| format!("Authorization: {value}\r\n"));
    let request = format!(
        "GET /watch HTTP/1.1\r\nHost: {address}\r\nUpgrade: websocket\r\n\
         Connection: keep-alive, Upgrade\r\nSec-WebSocket-Key: {KEY}\r\n\
         Sec-WebSocket-Version: 13\r\n{offered}\
         Sec-WebSocket-Extensions: permessage-deflate\r\n\r\n"
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

    // The joined text is `{"follow":{"from":0}}` — well-formed JSON and not a
    // follow request. What proves the reassembly is that the refusal names the
    // field the *joined* message held: either fragment alone would not parse,
    // and a wrong join would name something else.
    let (_, opcode, payload) = receive(&mut stream);
    assert_eq!(opcode, 1, "the reassembled message got no answer in words");
    let text = String::from_utf8(payload).expect("a text frame carries text");
    assert!(
        text.contains("follow"),
        "the refusal does not quote the field the joined message held, so the \
         fragments were not reassembled into one message: {text}"
    );

    let (_, opcode, payload) = receive(&mut stream);
    assert_eq!(opcode, 8, "the socket stayed open after a refused request");
    assert_eq!(u16::from_be_bytes([payload[0], payload[1]]), 1008);
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

/// Run a script over a **separate** HTTP connection, and answer its status.
///
/// Separate on purpose: the point of the test below is that a change made
/// somewhere else arrives here, and a helper that shared the socket would prove
/// nothing about that.
fn script(address: &str, source: &str) -> u16 {
    script_as(address, source, None)
}

/// Run a script as somebody, over a separate connection.
fn script_as(address: &str, source: &str, credential: Option<&str>) -> u16 {
    let mut stream = TcpStream::connect(address).unwrap();
    stream.set_read_timeout(Some(PATIENCE)).unwrap();
    let offered =
        credential.map_or_else(String::new, |value| format!("Authorization: {value}\r\n"));
    write!(
        stream,
        "POST /script HTTP/1.1\r\nHost: {address}\r\nContent-Length: {}\r\n{offered}Connection: close\r\n\r\n{source}",
        source.len()
    )
    .unwrap();
    stream.flush().unwrap();
    head(&mut stream).0
}

const READY: &str = "DEFINE NAMESPACE prod; USE NAMESPACE prod; \
                     DEFINE DATABASE library; USE DATABASE library; DEFINE TABLE users;";

#[test]
fn a_change_committed_on_another_connection_arrives_as_a_frame_on_this_one() {
    // Criterion F4, and the whole reason this task exists. A test that only
    // checked the follow request was accepted would pass with no push at all,
    // which is precisely the failure this suite keeps finding.
    let (_node, address) = node();
    assert_eq!(script(&address, READY), 200, "the fixture did not build");

    let (mut stream, status, _) = upgrade(&address);
    assert_eq!(status, 101);
    send(
        &mut stream,
        true,
        1,
        br#"{"namespace":"prod","database":"library","from":0,"table":"users"}"#,
    );

    // The commit happens on a different connection, after this socket is
    // already following.
    assert_eq!(
        script(
            &address,
            "USE NAMESPACE prod; USE DATABASE library; CREATE users:1 = { name: 'ada' };"
        ),
        200,
        "the write that should have been pushed did not happen"
    );

    let (_, opcode, payload) = receive(&mut stream);
    assert_eq!(opcode, 1, "a change must arrive as a text frame");
    let text = String::from_utf8(payload).expect("a text frame carries text");
    assert!(
        text.contains(r#""table":"users""#),
        "the frame does not name the table that changed: {text}"
    );
    assert!(
        text.contains(r#""became":"written""#),
        "a creation must arrive as a write: {text}"
    );
    assert!(
        text.contains("ada"),
        "the change arrived without the value that was written: {text}"
    );
}

#[test]
fn a_follow_request_naming_no_database_is_refused_in_words() {
    // A close code is five bits of meaning. "you named no database" and "that
    // table is not yours" are different things a subscriber must tell apart.
    let (_node, address) = node();
    let (mut stream, _, _) = upgrade(&address);
    send(&mut stream, true, 1, br#"{"namespace":"prod"}"#);

    let (_, opcode, payload) = receive(&mut stream);
    assert_eq!(opcode, 1, "a refusal must arrive as text, not only a close");
    let text = String::from_utf8(payload).expect("a text frame carries text");
    assert!(
        text.contains("database"),
        "the refusal must say what was missing: {text}"
    );
}

#[test]
fn a_binary_message_is_refused_with_the_code_that_says_which_kind_was_wrong() {
    // The opcode wave 53 deliberately dropped, now that there is a reader for
    // it. Without it a binary message would be parsed as if it were text.
    let (_node, address) = node();
    let (mut stream, _, _) = upgrade(&address);
    send(&mut stream, true, 2, b"\x00\x01\x02");

    let (_, opcode, payload) = receive(&mut stream);
    assert_eq!(opcode, 8, "a binary follow request must end the connection");
    assert_eq!(
        u16::from(payload[0]) << 8 | u16::from(payload[1]),
        1003,
        "the close must say the kind was wrong, not merely that something was"
    );
}

#[test]
fn a_namespace_that_could_carry_syntax_is_refused_before_a_statement_exists() {
    // The follow request reaches a `USE`, so it is guarded by the same narrow
    // rule the object routes use. If it were interpolated, this would run.
    let (_node, address) = node();
    assert_eq!(script(&address, READY), 200);

    let (mut stream, _, _) = upgrade(&address);
    send(
        &mut stream,
        true,
        1,
        br#"{"namespace":"prod; DROP TABLE users","database":"library"}"#,
    );

    let (_, opcode, payload) = receive(&mut stream);
    assert_eq!(opcode, 1, "the refusal must arrive as text");
    let text = String::from_utf8(payload).expect("a text frame carries text");
    assert!(
        text.contains("plain names"),
        "a namespace carrying syntax must be refused as a name: {text}"
    );

    // And the table is still there, which is the claim that actually matters.
    assert_eq!(
        script(
            &address,
            "USE NAMESPACE prod; USE DATABASE library; SELECT * FROM users;"
        ),
        200,
        "the interpolated statement ran and took the table with it"
    );
}

// ------------------------------------------------- whose grants the feed uses

// The filtering itself is proven at the core, by `bgv-db-wire/tests/pushing.rs`,
// which exercises grants, tenancy and field visibility and passed **unchanged**
// when that middle moved into `bgv_db::feed`. What none of it touches is which
// *session* this surface hands the core: every socket test above signs in
// nobody, against an open store.
//
// The failure that leaves open is narrow and not imaginary — a socket that
// signed a user in and then followed with a session that had not actually taken
// the sign-in would filter perfectly against the wrong identity, and every one
// of those tests would still pass. A subscription reaches records without ever
// running a statement, so whatever authorization it gets, it gets here.
//
// Three arms can present an identity and each ends at the same `sign_in`, so
// what is tested below is the *selection*: the follow message, the request head,
// and neither.

/// Credentials as a client sends them. Written out rather than computed, so a
/// change to the decoder cannot quietly agree with itself in both directions.
const OWNER: &str = "Basic cm9vdDpyb290IHNlY3JldA=="; // root:root secret
const SCOPED: &str = "Basic YWRhOmNvcnJlY3QgaG9yc2UgYmF0dGVyeQ=="; // ada:correct horse battery

/// A node whose store is closed, where `ada` may read `users` and not `ledger`.
fn granted() -> (Arc<Node>, String) {
    let (node, address) = node();
    // The owner is defined last: defining one is what closes the store, so
    // everything ahead of it in this script still runs without a credential.
    assert_eq!(
        script(
            &address,
            "DEFINE NAMESPACE prod; USE NAMESPACE prod; \
             DEFINE DATABASE library; USE DATABASE library; \
             DEFINE TABLE users; DEFINE TABLE ledger; \
             DEFINE USER root ROLE owner PASSWORD 'root secret';"
        ),
        200,
        "the fixture did not build"
    );
    assert_eq!(
        script_as(
            &address,
            "USE NAMESPACE prod; USE DATABASE library; \
             DEFINE USER ada ON prod.library ROLE editor PASSWORD 'correct horse battery'; \
             GRANT read ON users TO ada;",
            Some(OWNER)
        ),
        200,
        "the scoped user or the grant did not take"
    );
    (node, address)
}

/// Write a record as the owner, and answer whether the node took it.
fn wrote(address: &str, statement: &str) -> u16 {
    script_as(
        address,
        &format!("USE NAMESPACE prod; USE DATABASE library; {statement}"),
        Some(OWNER),
    )
}

#[test]
fn a_subscriber_is_told_only_about_the_tables_its_credential_was_granted() {
    // Q-94. The credential travels in the follow message, which is the arm a
    // browser has to use: `WebSocket` gives JavaScript no way to set a header.
    let (_node, address) = granted();
    let (mut stream, status, _) = upgrade(&address);
    assert_eq!(status, 101);
    send(
        &mut stream,
        true,
        1,
        br#"{"namespace":"prod","database":"library","from":0,"user":"ada","password":"correct horse battery"}"#,
    );

    // The ungranted table is written FIRST, so a feed that filtered nothing
    // would deliver it first. That ordering is this test's whole content — with
    // the writes the other way round it would pass without filtering anything.
    assert_eq!(wrote(&address, "CREATE ledger:1 = { total: 3 };"), 200);
    assert_eq!(wrote(&address, "CREATE users:1 = { name: 'ada' };"), 200);

    let (_, opcode, payload) = receive(&mut stream);
    assert_eq!(opcode, 1, "a change must arrive as a text frame");
    let text = String::from_utf8(payload).expect("a text frame carries text");
    assert!(
        !text.contains("ledger"),
        "the feed handed this subscriber a table nobody granted it: {text}"
    );
    assert!(
        text.contains(r#""table":"users""#),
        "the granted table's change did not arrive: {text}"
    );
}

#[test]
fn a_credential_in_the_upgrade_head_is_the_one_the_feed_filters_against() {
    // The other arm. The follow message below carries no credential at all, so
    // the request head is the only thing that can have signed anybody in.
    let (_node, address) = granted();
    let (mut stream, status, _) = upgrade_as(&address, Some(SCOPED));
    assert_eq!(status, 101);
    send(
        &mut stream,
        true,
        1,
        br#"{"namespace":"prod","database":"library","from":0,"table":"ledger"}"#,
    );

    let (_, opcode, payload) = receive(&mut stream);
    assert_eq!(opcode, 1, "the refusal must arrive as text");
    let text = String::from_utf8(payload).expect("a text frame carries text");
    // Refused *in the grant's own words*. A header that never reached `sign_in`
    // would leave an anonymous session, which fails somewhere else and says
    // something else — so "was it refused at all" is the weaker assertion that
    // would pass either way.
    assert!(
        text.contains("ledger") && text.contains("granted to read"),
        "the refusal did not come from the grant, so the credential in the \
         request head never reached the session the feed runs under: {text}"
    );
}

#[test]
fn an_anonymous_subscriber_is_told_nothing_by_a_closed_store() {
    // `/watch` has no 401 gate of its own — unlike `POST /script`, which the
    // router guards — so this rests entirely on the session the feed is handed.
    // The record is written BEFORE the subscribe and the follow starts at 0, so
    // it is already in the backlog: nothing here depends on timing.
    let (_node, address) = granted();
    assert_eq!(wrote(&address, "CREATE users:1 = { name: 'ada' };"), 200);

    let (mut stream, status, _) = upgrade(&address);
    assert_eq!(status, 101);
    send(
        &mut stream,
        true,
        1,
        br#"{"namespace":"prod","database":"library","from":0}"#,
    );

    let (_, opcode, payload) = receive(&mut stream);
    assert_eq!(opcode, 1, "the answer must arrive as text");
    let text = String::from_utf8(payload).expect("a text frame carries text");
    // The claim that matters, asserted before the cheaper one so a failure of
    // this one cannot be taken by the other.
    assert!(
        !text.contains(r#""became""#),
        "a subscriber that presented no credential was handed a change out of a \
         closed store: {text}"
    );
    // Delivering nothing would be equally safe, and this deliberately does not
    // accept it: a feed that goes silent leaves an operator with no way to tell
    // "not permitted" from "nothing has happened yet".
    assert!(
        text.contains("signed-in"),
        "the subscriber was not told why it is following nothing: {text}"
    );
}
