//! Signing in once, over a real socket.
//!
//! Exercised through a client rather than by calling the handlers, for the same
//! reason `routes.rs` is: the claim being tested is that a *caller* can sign in
//! once and stop sending a password, and a handler called directly proves only
//! that a function works.
//!
//! The properties under test are the ones that decide whether handing out a
//! token is safe at all — that it stands for exactly the identity that proved
//! itself, that it stops standing for anything the moment that identity changes,
//! and that it cannot be spent on getting another one.

#![allow(clippy::panic, clippy::unwrap_used, clippy::indexing_slicing)]

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::sync::Arc;

use tessari_http::Node;
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

/// One request, with an optional `Authorization` value.
fn send(
    address: &str,
    method: &str,
    path: &str,
    body: &str,
    credential: Option<&str>,
) -> (u16, String) {
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

/// `name:password`, base64, as a `Basic` header value.
fn basic(name: &str, password: &str) -> String {
    const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let raw = format!("{name}:{password}");
    let bytes = raw.as_bytes();
    let mut out = String::new();
    for chunk in bytes.chunks(3) {
        let held = u32::from(chunk[0]) << 16
            | u32::from(chunk.get(1).copied().unwrap_or(0)) << 8
            | u32::from(chunk.get(2).copied().unwrap_or(0));
        for index in 0_u32..4 {
            if usize::try_from(index).unwrap() <= chunk.len() {
                let shift = 18_u32.saturating_sub(6_u32.saturating_mul(index));
                let sextet = usize::try_from((held >> shift) & 0x3f).unwrap();
                out.push(char::from(ALPHABET[sextet]));
            } else {
                out.push('=');
            }
        }
    }
    format!("Basic {out}")
}

const PASSWORD: &str = "correct horse battery";

/// A store with a tenancy and two users, and the address it is served on.
fn peopled() -> (Arc<Node>, String) {
    let (node, address) = node();
    let (status, body) = send(
        &address,
        "POST",
        "/script",
        "DEFINE NAMESPACE prod; USE NAMESPACE prod; \
         DEFINE DATABASE shop; USE DATABASE shop; DEFINE TABLE orders; \
         DEFINE USER root ROLE owner PASSWORD 'correct horse battery';",
        None,
    );
    assert_eq!(status, 200, "{body}");
    let (status, body) = send(
        &address,
        "POST",
        "/script",
        "DEFINE USER ada ON prod.shop ROLE editor PASSWORD 'correct horse battery';",
        Some(&basic("root", PASSWORD)),
    );
    assert_eq!(status, 200, "{body}");
    (node, address)
}

/// The token out of a `POST /session` answer.
fn token_in(body: &str) -> String {
    let at = body.find(r#""token":""#).expect("a token in the answer");
    let rest = &body[at.saturating_add(r#""token":""#.len())..];
    let end = rest.find('"').expect("the token to end");
    rest[..end].to_owned()
}

/// A statement `ada` may run: her own tenancy, her own table.
///
/// Chosen deliberately over something like `INFO FOR NODE`, which an editor of
/// one database may *not* run — a revocation test whose statement was already
/// refused before the revocation proves nothing about the revocation.
const ADAS_OWN: &str = "USE NAMESPACE prod DATABASE shop; SELECT * FROM orders;";

/// Sign in and take the token.
fn opened(address: &str, name: &str) -> String {
    let (status, body) = send(
        address,
        "POST",
        "/session",
        "",
        Some(&basic(name, PASSWORD)),
    );
    assert_eq!(status, 200, "{body}");
    token_in(&body)
}

#[test]
fn a_password_is_spent_once_and_a_token_serves_afterwards() {
    let (_node, address) = peopled();
    let token = opened(&address, "ada");

    // Three requests, no password on any of them. This is the whole feature:
    // before it, each of these paid for an Argon2 hash.
    for order in 1..=3_u32 {
        let (status, body) = send(
            &address,
            "POST",
            "/script",
            &format!(
                "USE NAMESPACE prod DATABASE shop; CREATE orders:{order} = {{ total: {order} }};"
            ),
            Some(&format!("Bearer {token}")),
        );
        assert_eq!(status, 200, "{body}");
    }
}

#[test]
fn a_token_stands_for_the_identity_that_proved_itself_and_no_more() {
    let (_node, address) = peopled();
    let token = opened(&address, "ada");

    // `ada` edits inside one database and does not administer users. A token
    // that widened anything would be a second permission system quietly
    // disagreeing with the first.
    let (status, body) = send(
        &address,
        "POST",
        "/script",
        "DEFINE USER mallory ROLE owner PASSWORD 'a long enough one';",
        Some(&format!("Bearer {token}")),
    );
    assert_ne!(
        status, 200,
        "an editor's token must not declare users: {body}"
    );
}

#[test]
fn a_token_this_node_never_issued_is_refused_and_names_no_reason() {
    let (_node, address) = peopled();
    // Well-formed and wrong, which is the only shape worth testing: a malformed
    // header is already "no credential at all".
    let (status, body) = send(
        &address,
        "POST",
        "/script",
        "USE NAMESPACE prod DATABASE shop; SELECT * FROM orders;",
        Some(&format!("Bearer {}", "0".repeat(64))),
    );
    assert_eq!(status, 401, "{body}");
    // A store that told a token holder *why* would be telling somebody with a
    // stolen token what happened to the account they stole it from.
    assert!(!body.contains("ada"), "{body}");
    assert!(!body.contains("root"), "{body}");
}

#[test]
fn a_rotated_password_kills_the_token_that_was_issued_before_it() {
    let (_node, address) = peopled();
    let token = opened(&address, "ada");
    let bearer = format!("Bearer {token}");

    let (status, _) = send(&address, "POST", "/script", ADAS_OWN, Some(&bearer));
    assert_eq!(status, 200, "the token works before the rotation");

    let (status, body) = send(
        &address,
        "POST",
        "/script",
        "ALTER USER ada SET PASSWORD 'a different horse entirely';",
        Some(&basic("root", PASSWORD)),
    );
    assert_eq!(status, 200, "{body}");

    // Rotating a password somebody else may have learned is worthless if a
    // token minted with the old one keeps working.
    let (status, body) = send(&address, "POST", "/script", ADAS_OWN, Some(&bearer));
    assert_eq!(status, 401, "{body}");
}

#[test]
fn a_demotion_kills_the_token_that_was_issued_before_it() {
    let (_node, address) = peopled();
    let token = opened(&address, "ada");
    let bearer = format!("Bearer {token}");

    let (status, body) = send(
        &address,
        "POST",
        "/script",
        "ALTER USER ada SET ROLE viewer;",
        Some(&basic("root", PASSWORD)),
    );
    assert_eq!(status, 200, "{body}");

    // Otherwise a demotion is a note in the catalog rather than a change in
    // what somebody may do — for as long as they hold a token.
    let (status, body) = send(&address, "POST", "/script", ADAS_OWN, Some(&bearer));
    assert_eq!(status, 401, "{body}");
}

#[test]
fn removing_a_user_kills_their_token() {
    let (_node, address) = peopled();
    let token = opened(&address, "ada");
    let bearer = format!("Bearer {token}");

    let (status, body) = send(
        &address,
        "POST",
        "/script",
        "DROP USER ada;",
        Some(&basic("root", PASSWORD)),
    );
    assert_eq!(status, 200, "{body}");

    let (status, body) = send(&address, "POST", "/script", ADAS_OWN, Some(&bearer));
    assert_eq!(status, 401, "{body}");
}

#[test]
fn a_token_cannot_be_spent_on_another_token() {
    let (_node, address) = peopled();
    let token = opened(&address, "ada");

    // Rolling one token into the next would make expiry meaningless: a holder
    // could keep it alive for as long as they kept asking, and the account
    // would never come back under the control of whoever owns the password.
    let (status, body) = send(
        &address,
        "POST",
        "/session",
        "",
        Some(&format!("Bearer {token}")),
    );
    assert_eq!(status, 401, "{body}");
}

#[test]
fn a_session_can_be_handed_back() {
    let (_node, address) = peopled();
    let token = opened(&address, "ada");
    let bearer = format!("Bearer {token}");

    let (status, _) = send(&address, "DELETE", "/session", "", Some(&bearer));
    assert_eq!(status, 200);

    // A credential a client cannot hand back is one it holds until it exits.
    let (status, body) = send(&address, "POST", "/script", ADAS_OWN, Some(&bearer));
    assert_eq!(status, 401, "{body}");
}

#[test]
fn an_open_store_hands_out_no_token() {
    let (_node, address) = node();
    // Nobody has been declared, so anybody may do anything — and a token minted
    // out of that would still be live after the first `DEFINE USER` closed the
    // store, which is a key that outlives the lock.
    let (status, body) = send(&address, "POST", "/session", "", Some(&basic("", "")));
    assert_ne!(status, 200, "{body}");
}

#[test]
fn the_scrape_says_how_many_sessions_are_held() {
    let (_node, address) = peopled();
    let (status, body) = send(&address, "GET", "/metrics", "", None);
    assert_eq!(status, 200);
    assert!(body.contains("tessari_sessions 0"), "{body}");

    let _token = opened(&address, "ada");
    let (_, body) = send(&address, "GET", "/metrics", "", None);
    // The one number that says whether the token bound is close: a node at the
    // ceiling refuses sign-ins while every other counter still reads healthy.
    assert!(body.contains("tessari_sessions 1"), "{body}");
}
