//! A space over HTTP — `/kv/{ns}/{db}/{space}/{op}/{key…}` (ADR-0090, G046 C3).
//!
//! Each route is a surface over one space statement, so the cases here are the
//! ones where that could stop being true: a condition that did not hold answered
//! as an error, a typed value narrowed on the way in, a key read as an operation,
//! a lock released by a plain write that makes it permanent, and a second
//! permission model.

#![allow(clippy::panic, clippy::unwrap_used, clippy::indexing_slicing)]

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::sync::Arc;

use tessari_http::Node;
use tessaridb::Db;

fn node() -> String {
    let db = Arc::new(Db::in_memory().unwrap());
    let node = Arc::new(Node::bind(db, "127.0.0.1:0").unwrap());
    let address = node.address();
    std::thread::spawn(move || crate::serve_until_the_test_ends(&node));
    address
}

/// One request, and the status and body it answered with.
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
    let mut reader = BufReader::new(stream);
    let mut status_line = String::new();
    reader.read_line(&mut status_line).unwrap();
    let status = status_line
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
    let mut body = String::new();
    reader.read_to_string(&mut body).unwrap();
    (status, body)
}

fn call(address: &str, method: &str, path: &str, body: &str) -> (u16, String) {
    send(address, method, path, body, None)
}

const READY: &str = "DEFINE NAMESPACE app; USE NAMESPACE app; DEFINE DATABASE main; \
                     USE DATABASE main; DEFINE SPACE cache; DEFINE COLLECTION notes;";

fn ready() -> String {
    let address = node();
    assert_eq!(call(&address, "POST", "/script", READY).0, 200);
    address
}

const KEY: &str = "/kv/app/main/cache/key";

#[test]
fn a_value_written_with_an_expiry_reads_back_with_its_types_and_its_ttl() {
    let address = ready();
    let written = call(
        &address,
        "PUT",
        &format!("{KEY}/user%3A42?expire=30s"),
        "{ at: datetime '2026-09-30T10:00:00Z', price: dec 12.34, wait: 1h30m }",
    );
    assert_eq!(written, (200, r#"{"written":true}"#.to_owned()));

    let (status, body) = call(&address, "GET", &format!("{KEY}/user%3A42"), "");
    assert_eq!(status, 200, "{body}");
    assert!(
        body.contains(r#""at":"2026-09-30T10:00:00Z""#) && body.contains(r#""price":"12.34""#),
        "the value did not come back as it was written: {body}"
    );
    assert!(
        !body.contains(r#""ttl":null"#) && body.contains(r#""ttl":""#),
        "a value written with an expiry reads back as never expiring: {body}"
    );

    // Written with no expiry, it never expires — and a plain write clears one.
    call(&address, "PUT", &format!("{KEY}/user%3A42"), "1");
    let (_, body) = call(&address, "GET", &format!("{KEY}/user%3A42"), "");
    assert_eq!(body, r#"{"value":1,"ttl":null}"#);
}

#[test]
fn a_key_that_names_an_operation_is_still_a_key() {
    let address = ready();
    assert_eq!(
        call(&address, "PUT", &format!("{KEY}/a/incr/b"), "'x'").0,
        200
    );
    let (status, body) = call(&address, "GET", &format!("{KEY}/a/incr/b"), "");
    assert_eq!(
        (status, body.as_str()),
        (200, r#"{"value":"x","ttl":null}"#)
    );
    assert_eq!(
        call(&address, "GET", "/kv/app/main/cache?prefix=a%2F", "").1,
        r#"{"keys":["a/incr/b"]}"#
    );
}

#[test]
fn a_condition_that_does_not_hold_is_false_and_not_an_error() {
    let address = ready();
    let absent = format!("{KEY}/k?if=absent");
    assert_eq!(
        call(&address, "PUT", &absent, "1"),
        (200, r#"{"written":true}"#.to_owned())
    );
    assert_eq!(
        call(&address, "PUT", &absent, "2"),
        (200, r#"{"written":false}"#.to_owned())
    );
    assert_eq!(
        call(&address, "PUT", &format!("{KEY}/nobody?if=present"), "1"),
        (200, r#"{"written":false}"#.to_owned())
    );
    let swap = "/kv/app/main/cache/swap/k";
    assert_eq!(
        call(&address, "POST", swap, "{ expect: 9, value: 3 }"),
        (200, r#"{"written":false}"#.to_owned())
    );
    assert_eq!(
        call(&address, "POST", swap, "{ expect: 1, value: 3 }"),
        (200, r#"{"written":true}"#.to_owned())
    );
    assert_eq!(
        call(&address, "GET", &format!("{KEY}/k"), "").1,
        r#"{"value":3,"ttl":null}"#
    );
}

#[test]
fn delete_says_whether_there_was_a_key() {
    let address = ready();
    call(&address, "PUT", &format!("{KEY}/gone"), "NULL");
    assert_eq!(
        call(&address, "DELETE", &format!("{KEY}/gone"), ""),
        (200, r#"{"deleted":true}"#.to_owned()),
        "a key holding NULL is a key"
    );
    assert_eq!(
        call(&address, "DELETE", &format!("{KEY}/gone"), ""),
        (200, r#"{"deleted":false}"#.to_owned())
    );
    assert_eq!(call(&address, "GET", &format!("{KEY}/gone"), "").0, 404);
}

#[test]
fn incr_counts_from_zero_and_answers_the_new_value() {
    let address = ready();
    let path = "/kv/app/main/cache/incr/hits?by=5";
    assert_eq!(
        call(&address, "POST", path, ""),
        (200, r#"{"value":5}"#.to_owned())
    );
    assert_eq!(
        call(&address, "POST", path, ""),
        (200, r#"{"value":10}"#.to_owned())
    );
    assert_eq!(
        call(&address, "POST", "/kv/app/main/cache/incr/hits", ""),
        (200, r#"{"value":11}"#.to_owned())
    );
}

#[test]
fn expire_and_persist_move_a_keys_expiry_and_say_whether_it_was_there() {
    let address = ready();
    call(&address, "PUT", &format!("{KEY}/s"), "'v'");
    assert_eq!(
        call(
            &address,
            "POST",
            "/kv/app/main/cache/expire/s?expire=10m",
            ""
        ),
        (200, r#"{"found":true}"#.to_owned())
    );
    assert!(
        !call(&address, "GET", &format!("{KEY}/s"), "")
            .1
            .contains(r#""ttl":null"#)
    );
    assert_eq!(
        call(&address, "POST", "/kv/app/main/cache/persist/s", ""),
        (200, r#"{"found":true}"#.to_owned())
    );
    assert!(
        call(&address, "GET", &format!("{KEY}/s"), "")
            .1
            .contains(r#""ttl":null"#)
    );
    assert_eq!(
        call(
            &address,
            "POST",
            "/kv/app/main/cache/expire/nobody?expire=10m",
            ""
        ),
        (200, r#"{"found":false}"#.to_owned())
    );
}

#[test]
fn a_lock_is_a_lease_that_its_holder_extends_and_releases_and_nobody_else_can() {
    let address = ready();
    let lock = |holder: &str| {
        call(
            &address,
            "POST",
            &format!("/kv/app/main/cache/lock/report?holder={holder}&expire=30s"),
            "",
        )
    };
    let unlock = |holder: &str| {
        call(
            &address,
            "POST",
            &format!("/kv/app/main/cache/unlock/report?holder={holder}"),
            "",
        )
    };
    assert_eq!(lock("a"), (200, r#"{"held":true}"#.to_owned()));
    assert_eq!(
        lock("b"),
        (200, r#"{"held":false}"#.to_owned()),
        "a held lock was taken"
    );
    assert_eq!(
        lock("a"),
        (200, r#"{"held":true}"#.to_owned()),
        "its holder could not extend it"
    );
    assert_eq!(
        unlock("b"),
        (200, r#"{"released":false}"#.to_owned()),
        "another holder released it"
    );
    assert_eq!(unlock("a"), (200, r#"{"released":true}"#.to_owned()));

    // Released by an expiring write, so the next holder can take it: a hand-back
    // without an expiry would leave the key there for ever.
    std::thread::sleep(std::time::Duration::from_millis(20));
    assert_eq!(
        lock("b"),
        (200, r#"{"held":true}"#.to_owned()),
        "a released lock could not be taken again, so the release left it permanent"
    );
}

#[test]
fn keys_are_walked_by_prefix_after_a_key_and_up_to_a_limit() {
    let address = ready();
    for key in ["user%3A1", "user%3A2", "other%3A1"] {
        call(&address, "PUT", &format!("{KEY}/{key}"), "1");
    }
    let list = |query: &str| call(&address, "GET", &format!("/kv/app/main/cache{query}"), "");
    assert_eq!(list("?prefix=user%3A").1, r#"{"keys":["user:1","user:2"]}"#);
    assert_eq!(list("?prefix=user%3A&limit=1").1, r#"{"keys":["user:1"]}"#);
    assert_eq!(
        list("?prefix=user%3A&after=user%3A1").1,
        r#"{"keys":["user:2"]}"#
    );
    assert_eq!(list("?limit=0").0, 400);
    assert_eq!(list("?limit=1001").0, 400);
}

#[test]
fn what_is_not_a_space_or_not_a_request_is_refused_before_anything_runs() {
    let address = ready();
    assert_eq!(
        call(&address, "GET", "/kv/app/main/notes/key/x", "").0,
        404,
        "a collection"
    );
    assert_eq!(
        call(&address, "PUT", "/kv/app/main/notes/key/x", "1").0,
        404,
        "a write aimed at a collection was not refused as not-a-space"
    );
    assert_eq!(
        call(&address, "GET", "/kv/app/main/nothere/key/x", "").0,
        404,
        "nothing"
    );
    assert_eq!(
        call(&address, "GET", "/kv/app/ma-in/cache/key/x", "").0,
        400,
        "a bad name"
    );
    assert_eq!(
        call(&address, "GET", "/kv/app/main/cache/frob/x", "").0,
        404,
        "no such op"
    );
    assert_eq!(
        call(&address, "PUT", &format!("{KEY}/x?expire=abc"), "1").0,
        400
    );
    assert_eq!(
        call(&address, "PUT", &format!("{KEY}/x?expire=-5s"), "1").0,
        400
    );
    assert_eq!(
        call(&address, "PUT", &format!("{KEY}/x?if=sometimes"), "1").0,
        400
    );
    assert_eq!(
        call(&address, "PUT", &format!("{KEY}/x"), "SELECT * FROM notes").0,
        400
    );
    assert_eq!(call(&address, "POST", &format!("{KEY}/x"), "1").0, 405);
    assert_eq!(
        call(&address, "POST", "/kv/app/main/cache/lock/x?expire=30s", "").0,
        400,
        "no holder"
    );
    assert_eq!(
        call(&address, "GET", &format!("{KEY}/x"), "").0,
        404,
        "nothing above wrote"
    );
}

#[test]
fn a_closed_store_asks_for_a_credential_and_a_grant_like_every_route() {
    let address = ready();
    let owner = Some("Basic b3duZXI6b3duZXItcGFzcw==");
    let reader = Some("Basic cmVhZGVyOnJlYWRlci1wYXNz");
    let declared = send(
        &address,
        "POST",
        "/script",
        "DEFINE USER owner ROLE owner PASSWORD 'owner-pass';",
        None,
    );
    assert_eq!(declared.0, 200, "{}", declared.1);
    let declared = send(
        &address,
        "POST",
        "/script",
        "DEFINE USER reader ON app.main ROLE viewer PASSWORD 'reader-pass';",
        owner,
    );
    assert_eq!(declared.0, 200, "{}", declared.1);

    assert_eq!(send(&address, "PUT", &format!("{KEY}/x"), "1", None).0, 401);
    assert_eq!(
        send(&address, "PUT", &format!("{KEY}/x"), "1", reader).0,
        403
    );
    assert_eq!(
        send(&address, "PUT", &format!("{KEY}/x"), "1", owner).0,
        200
    );
    assert_eq!(
        send(&address, "GET", &format!("{KEY}/x"), "", reader).0,
        200
    );
}
