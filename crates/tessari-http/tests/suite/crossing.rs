//! Two real tenants, and a caller who belongs to one asking for the other.
//!
//! # Attempted, not inspected
//!
//! The distinction is the whole file. A test that reads its own tenancy and
//! counts the rows proves a filter ran; only a test that asks `prod.shop` for
//! `staging.shop`'s record proves the answer is a **refusal** rather than an
//! empty list — and empty and refused are different answers, of which only one
//! survives the filter being dropped. Every case here is a crossing the store
//! must say no to, and every one is paired with the same request inside the
//! caller's own tenancy, so a surface that had simply stopped working could not
//! pass.
//!
//! # Why the tenancies are named the way they are
//!
//! `prod.shop` and `staging.shop` share a database name on purpose. A store that
//! confined by database name rather than by the resolved container would let
//! `staging.shop` answer for `prod.shop`, and a fixture with two distinct names
//! would never notice.

#![allow(clippy::panic, clippy::unwrap_used, clippy::indexing_slicing)]

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::sync::Arc;

use tessari_http::Node;
use tessaridb::Db;

/// `root:a long one` and `nina:a long one`, in base64.
const ROOT: &str = "Basic cm9vdDphIGxvbmcgb25l";
const NINA: &str = "Basic bmluYTphIGxvbmcgb25l";

fn node() -> (Arc<Node>, String) {
    let db = Arc::new(Db::in_memory().unwrap());
    let node = Arc::new(Node::bind(db, "127.0.0.1:0").unwrap());
    let address = node.address();
    let serving = Arc::clone(&node);
    std::thread::spawn(move || serving.serve());
    (node, address)
}

fn send(
    address: &str,
    method: &str,
    path: &str,
    body: &[u8],
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
    (status, String::from_utf8_lossy(&answered).into_owned())
}

fn script(address: &str, source: &str, credential: Option<&str>) -> (u16, String) {
    send(address, "POST", "/script", source.as_bytes(), credential)
}

/// Two tenancies, each with the same database and bucket names, and a user who
/// owns one of them and nothing above it.
fn two_tenants(address: &str) {
    let (status, said) = script(
        address,
        "DEFINE NAMESPACE prod; USE NAMESPACE prod; DEFINE DATABASE shop; USE DATABASE shop; \
         DEFINE COLLECTION orders; CREATE orders:1 = { total: 5 }; DEFINE BUCKET media; \
         DEFINE USER root ROLE owner PASSWORD 'a long one';",
        None,
    );
    assert_eq!(status, 200, "{said}");
    let (status, said) = script(
        address,
        "DEFINE NAMESPACE staging; USE NAMESPACE staging; DEFINE DATABASE shop; \
         USE DATABASE shop; DEFINE COLLECTION orders; CREATE orders:1 = { total: 9 }; \
         DEFINE BUCKET media; \
         USE NAMESPACE prod; DEFINE DATABASE archive; USE DATABASE archive; \
         DEFINE COLLECTION orders; CREATE orders:1 = { total: 7 }; \
         USE DATABASE shop; \
         DEFINE USER nina ON prod.shop ROLE owner PASSWORD 'a long one';",
        Some(ROOT),
    );
    assert_eq!(status, 200, "{said}");
    // A file in each, so a refusal is never mistakable for a 404.
    for tenancy in ["prod", "staging"] {
        let (status, said) = send(
            address,
            "PUT",
            &format!("/files/{tenancy}/shop/media/note.txt"),
            b"bytes",
            Some(ROOT),
        );
        assert_eq!(status, 201, "{said}");
    }
}

#[test]
fn the_script_route_refuses_a_crossing_by_selection_and_by_name() {
    let (_node, address) = node();
    two_tenants(&address);

    // Her own, which is what makes the two refusals below mean something.
    let (status, said) = script(
        &address,
        "USE NAMESPACE prod; USE DATABASE shop; SELECT * FROM orders;",
        Some(NINA),
    );
    assert_eq!(status, 200, "her own tenancy was refused: {said}");

    // Selecting her way across.
    let (status, said) = script(&address, "USE NAMESPACE staging;", Some(NINA));
    assert!(status >= 400, "she selected another namespace: {said}");

    // And naming her way across, which never touches `USE` — a guard on the
    // front door of a room with two doors is not a guard. The name is a sibling
    // **database**, because that is the crossing the language can express: it
    // qualifies a table as `database.table` within the selected namespace and
    // has no three-part form, so the only door out of a namespace is `USE`.
    let (status, said) = script(
        &address,
        "USE NAMESPACE prod; USE DATABASE shop; SELECT * FROM archive.orders;",
        Some(NINA),
    );
    assert!(status >= 400, "she read a sibling database by name: {said}");
    // The field name rather than the value: a span in the refusal carries
    // digits of its own, and asserting on a digit would read one of those as
    // the record and pass or fail for the wrong reason.
    assert!(
        !said.contains("total"),
        "the refusal carried the sibling's record: {said}"
    );

    // The same statement as somebody the sibling *is* theirs: without it, a
    // refusal produced by a name nobody can parse would pass just as well.
    let (status, said) = script(
        &address,
        "USE NAMESPACE prod; USE DATABASE shop; SELECT * FROM archive.orders;",
        Some(ROOT),
    );
    assert_eq!(status, 200, "the name is not readable by anybody: {said}");
    assert!(said.contains("total"), "the control read nothing: {said}");
}

#[test]
fn the_backup_route_refuses_a_caller_who_would_take_a_tenancy_that_is_not_theirs() {
    // A backup is every record in the store, so for a tenant it is the widest
    // crossing there is: one request that would hand over every namespace.
    let (_node, address) = node();
    two_tenants(&address);

    let (status, said) = send(&address, "GET", "/backup", b"", Some(NINA));
    assert!(status >= 400, "a tenant downloaded the whole store: {said}");

    let (status, _) = send(&address, "GET", "/backup", b"", Some(ROOT));
    assert_eq!(status, 200, "the store owner was refused their own backup");
}

#[test]
fn the_file_routes_refuse_a_crossing_on_every_method_that_reads() {
    let (_node, address) = node();
    two_tenants(&address);

    // Her own bucket, by each reading method, so the crossings below are about
    // the tenancy rather than about the route.
    let (status, said) = send(
        &address,
        "GET",
        "/files/prod/shop/media/note.txt",
        b"",
        Some(NINA),
    );
    assert_eq!(status, 200, "her own file was refused: {said}");
    let (status, _) = send(
        &address,
        "HEAD",
        "/files/prod/shop/media/note.txt",
        b"",
        Some(NINA),
    );
    assert_eq!(status, 200, "her own file was refused a head");
    let (status, _) = send(&address, "GET", "/files/prod/shop/media", b"", Some(NINA));
    assert_eq!(status, 200, "her own bucket would not list");

    // The same three, one namespace across. The file is really there, so a
    // refusal cannot be a 404 wearing a different number.
    for (method, path) in [
        ("GET", "/files/staging/shop/media/note.txt"),
        ("HEAD", "/files/staging/shop/media/note.txt"),
        ("GET", "/files/staging/shop/media"),
    ] {
        let (status, said) = send(&address, method, path, b"", Some(NINA));
        assert!(
            status >= 400 && status != 404,
            "{method} {path} answered {status}: {said}"
        );
        assert!(
            !said.contains("bytes"),
            "{method} {path} handed over the file: {said}"
        );
    }
}

/// `ada:a long one`, in base64 — a viewer, and the lowest privilege that exists.
const ADA: &str = "Basic YWRhOmEgbG9uZyBvbmU=";

/// A store with an owner of everything and a viewer of one database.
///
/// The viewer is the probe's whole point. Every other case in this file asks
/// whether one tenant can reach another's data; these ask whether the least
/// privileged account this store can hold — one that may legitimately sign in,
/// read its own database, and nothing else — can reach the administration the
/// console's Users screen performs. The console is not in the loop for any of
/// it: a guard that lives in a disabled button is a guard against the button.
fn an_owner_and_a_viewer(address: &str) {
    let (status, said) = script(
        address,
        "DEFINE NAMESPACE prod; USE NAMESPACE prod; DEFINE DATABASE shop; USE DATABASE shop; \
         DEFINE COLLECTION orders; CREATE orders:1 = { total: 5 }; \
         DEFINE USER root ROLE owner PASSWORD 'a long one';",
        None,
    );
    assert_eq!(status, 200, "{said}");
    let (status, said) = script(
        address,
        "USE NAMESPACE prod; USE DATABASE shop; \
         DEFINE USER ada ON prod.shop ROLE viewer PASSWORD 'a long one';",
        Some(ROOT),
    );
    assert_eq!(status, 200, "{said}");
}

#[test]
fn a_viewer_is_refused_at_every_point_the_users_screen_reaches() {
    let (_node, address) = node();
    an_owner_and_a_viewer(&address);

    // The control first. Without it a store that had simply stopped answering
    // this account would pass every refusal below, which is the failure mode a
    // one-sided probe cannot see.
    let (status, said) = script(
        &address,
        "USE NAMESPACE prod; USE DATABASE shop; SELECT * FROM orders;",
        Some(ADA),
    );
    assert_eq!(
        status, 200,
        "the viewer could not read her own database: {said}"
    );

    // Each statement the console's Users screen sends, sent the way a `curl`
    // would send it. The screen's own guards — a disabled button, a typed
    // confirmation, a reason field — are not in this loop at all, which is the
    // point: they are deliberation aids for the operator and were never the
    // thing standing between a viewer and a `DROP USER`.
    for (what, statement) in [
        ("list the accounts", "INFO FOR USERS;"),
        (
            "define an account",
            "DEFINE USER mallory ROLE owner PASSWORD 'a long one';",
        ),
        ("promote herself", "ALTER USER ada SET ROLE owner;"),
        (
            "reset another account's password",
            "ALTER USER root SET PASSWORD 'a long one too';",
        ),
        ("remove the owner", "DROP USER root;"),
    ] {
        let (status, said) = script(&address, statement, Some(ADA));
        assert!(status >= 400, "a viewer could {what}: {status} {said}");
    }

    // And the owner can, so the refusals above are about the identity rather
    // than about a surface that has stopped working.
    let (status, said) = script(&address, "INFO FOR USERS;", Some(ROOT));
    assert_eq!(status, 200, "the owner was refused the listing: {said}");
    let (status, said) = script(&address, "ALTER USER ada SET ROLE editor;", Some(ROOT));
    assert_eq!(status, 200, "the owner was refused a role change: {said}");
}

#[test]
fn the_password_route_can_only_ever_change_the_caller_s_own() {
    // The console's eighth mutation, and the one that is not a statement. It
    // takes a credential and a new password and has no field for a subject, so
    // the question is not whether it checks one — it is whether the absence
    // holds when somebody tries. A confused deputy here would let any account
    // that can sign in take over any other.
    let (_node, address) = node();
    an_owner_and_a_viewer(&address);

    let (status, said) = send(&address, "POST", "/password", b"a new long one", Some(ADA));
    assert_eq!(
        status, 200,
        "the viewer could not change her own password: {said}"
    );

    // Hers moved.
    let (status, _) = script(
        &address,
        "USE NAMESPACE prod; USE DATABASE shop; SELECT * FROM orders;",
        Some(ADA),
    );
    assert_eq!(status, 401, "her old password still works");

    // The owner's did not, which is the assertion the whole case exists for.
    let (status, said) = script(&address, "INFO FOR USERS;", Some(ROOT));
    assert_eq!(
        status, 200,
        "the viewer's password change reached the owner's account: {said}"
    );
}
