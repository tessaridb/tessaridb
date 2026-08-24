//! Files over HTTP, through a real socket.
//!
//! The object routes exist so a caller who already knows how to `PUT` a file at
//! a URL does not have to learn a query language first. What they must **not**
//! get is a second permission model, so the tests that matter most here are the
//! refusals: an unauthenticated caller against a closed store, and an
//! authenticated one without the grant, answered differently and both answered
//! no.

#![allow(clippy::panic, clippy::unwrap_used, clippy::indexing_slicing)]

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::sync::Arc;

use tessari_http::Node;
use tessaridb::Db;

fn node() -> (Arc<Node>, String) {
    let db = Arc::new(Db::in_memory().unwrap());
    let node = Arc::new(Node::bind(db, "127.0.0.1:0").unwrap());
    let address = node.address();
    let serving = Arc::clone(&node);
    std::thread::spawn(move || serving.serve());
    (node, address)
}

/// One request with a byte body, and the status and bytes it answered with.
fn send(
    address: &str,
    method: &str,
    path: &str,
    body: &[u8],
    credential: Option<&str>,
) -> (u16, Vec<u8>) {
    let mut stream = TcpStream::connect(address).unwrap();
    let authorization =
        credential.map_or_else(String::new, |value| format!("Authorization: {value}\r\n"));
    let head = format!(
        "{method} {path} HTTP/1.1\r\nHost: {address}\r\n{authorization}Content-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    stream.write_all(head.as_bytes()).unwrap();
    stream.write_all(body).unwrap();
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
    let mut answered = Vec::new();
    reader.read_to_end(&mut answered).unwrap();
    (status, answered)
}

fn script(address: &str, source: &str, credential: Option<&str>) -> u16 {
    send(address, "POST", "/script", source.as_bytes(), credential).0
}

const READY: &str = "DEFINE NAMESPACE prod; USE NAMESPACE prod; \
                     DEFINE DATABASE library; USE DATABASE library; DEFINE BUCKET media;";

#[test]
fn a_file_goes_in_and_comes_back_byte_for_byte() {
    let (_node, address) = node();
    assert_eq!(script(&address, READY, None), 200);

    let bytes: Vec<u8> = (0..=255u8).cycle().take(5000).collect();
    let (status, _) = send(
        &address,
        "PUT",
        "/files/prod/library/media/photos/a.bin",
        &bytes,
        None,
    );
    assert_eq!(status, 201);

    let (status, answered) = send(
        &address,
        "GET",
        "/files/prod/library/media/photos/a.bin",
        b"",
        None,
    );
    assert_eq!(status, 200);
    assert_eq!(answered, bytes, "the bytes changed on the way through");
}

#[test]
fn a_path_that_looks_like_a_statement_is_a_file_name() {
    // The whole reason a path travels as a parameter. If it were interpolated,
    // this URL would drop a table.
    let (_node, address) = node();
    assert_eq!(script(&address, READY, None), 200);
    assert_eq!(
        script(
            &address,
            "USE NAMESPACE prod; USE DATABASE library; DEFINE TABLE users; \
             CREATE users:1 = { name: 'ada' };",
            None
        ),
        200
    );

    let (status, _) = send(
        &address,
        "PUT",
        "/files/prod/library/media/%27%3B%20DROP%20TABLE%20users%3B%20--",
        b"harmless",
        None,
    );
    assert_eq!(status, 201);

    // The table is still there, and the file is the one that was written.
    let (status, body) = send(
        &address,
        "POST",
        "/script",
        b"USE NAMESPACE prod; USE DATABASE library; SELECT * FROM users;",
        None,
    );
    assert_eq!(status, 200);
    assert!(
        String::from_utf8_lossy(&body).contains("ada"),
        "the path ran as a statement"
    );
}

#[test]
fn a_bucket_lists_the_files_it_holds() {
    let (_node, address) = node();
    assert_eq!(script(&address, READY, None), 200);
    for name in ["a.txt", "b.txt"] {
        send(
            &address,
            "PUT",
            &format!("/files/prod/library/media/{name}"),
            b"held",
            None,
        );
    }

    let (status, body) = send(&address, "GET", "/files/prod/library/media", b"", None);
    assert_eq!(status, 200);
    let body = String::from_utf8_lossy(&body);
    assert!(body.contains("/a.txt"), "{body}");
    assert!(body.contains("/b.txt"), "{body}");
    assert!(body.contains(r#""size""#), "{body}");
}

#[test]
fn deleting_a_file_leaves_nothing_to_get() {
    let (_node, address) = node();
    assert_eq!(script(&address, READY, None), 200);
    send(
        &address,
        "PUT",
        "/files/prod/library/media/gone.txt",
        b"held",
        None,
    );

    let (status, _) = send(
        &address,
        "DELETE",
        "/files/prod/library/media/gone.txt",
        b"",
        None,
    );
    assert_eq!(status, 204);

    let (status, _) = send(
        &address,
        "GET",
        "/files/prod/library/media/gone.txt",
        b"",
        None,
    );
    assert_eq!(status, 404);
}

#[test]
fn a_file_nobody_wrote_is_a_404_and_not_an_empty_body() {
    let (_node, address) = node();
    assert_eq!(script(&address, READY, None), 200);
    let (status, _) = send(
        &address,
        "GET",
        "/files/prod/library/media/never.txt",
        b"",
        None,
    );
    assert_eq!(status, 404);
}

#[test]
fn a_bucket_name_that_could_carry_syntax_is_refused_before_a_statement_exists() {
    let (_node, address) = node();
    assert_eq!(script(&address, READY, None), 200);
    let (status, _) = send(
        &address,
        "GET",
        "/files/prod/library/media;DROP/x.txt",
        b"",
        None,
    );
    assert_eq!(status, 400);
}

#[test]
fn a_closed_store_answers_401_without_a_credential_and_403_when_the_grant_forbids() {
    // The two refusals a client has to tell apart: one says "say who you are",
    // the other says "I know, and no". Both come from the session, so they are
    // the same refusals the language gives.
    let (_node, address) = node();
    assert_eq!(script(&address, READY, None), 200);
    assert_eq!(
        script(
            &address,
            "DEFINE USER root ROLE owner PASSWORD 'a long one';",
            None
        ),
        200
    );
    // `root:a long one` in base64.
    let root = "Basic cm9vdDphIGxvbmcgb25l";
    assert_eq!(
        script(
            &address,
            "USE NAMESPACE prod; USE DATABASE library; DEFINE TABLE notes; \
             DEFINE USER ada ON prod.library ROLE editor PASSWORD 'a long one'; \
             GRANT read ON notes TO ada;",
            Some(root)
        ),
        200
    );

    let (status, _) = send(
        &address,
        "GET",
        "/files/prod/library/media/anything.txt",
        b"",
        None,
    );
    assert_eq!(status, 401, "an unauthenticated caller reached a file");

    // `ada:a long one` in base64. Granted on `notes` and not on `media`.
    let ada = "Basic YWRhOmEgbG9uZyBvbmU=";
    let (status, _) = send(
        &address,
        "GET",
        "/files/prod/library/media/anything.txt",
        b"",
        Some(ada),
    );
    assert_eq!(status, 403, "a caller without the grant reached a file");
}
