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
            "USE NAMESPACE prod; USE DATABASE library; DEFINE COLLECTION users; \
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

/// A listing carries a file listing, and nothing about how the store found it
/// (Q-260).
///
/// The route used to answer with the raw statement result wrapped in a key:
/// `plan.access`, `plan.table`, `kind` and `path` are query-planner internals,
/// and `chunks` is a storage detail. Published as-is they become a contract for
/// every client in every language, and then changing the planner breaks clients
/// that never asked about it.
///
/// A file listing wants a name, a size and a modification time. The records
/// already carry exactly those three.
#[test]
fn a_listing_says_what_the_files_are_and_not_how_they_were_found() {
    let (_node, address) = node();
    assert_eq!(script(&address, READY, None), 200);
    send(
        &address,
        "PUT",
        "/files/prod/library/media/a.txt",
        b"held",
        None,
    );

    let (status, body) = send(&address, "GET", "/files/prod/library/media", b"", None);
    assert_eq!(status, 200);
    let body = String::from_utf8_lossy(&body);

    assert!(body.contains(r#""path":"/a.txt""#), "{body}");
    assert!(body.contains(r#""size":4"#), "{body}");
    assert!(body.contains(r#""updated""#), "{body}");

    for internal in [r#""plan""#, r#""access""#, r#""kind""#, r#""chunks""#] {
        assert!(
            !body.contains(internal),
            "the listing published {internal}, which is not a file's property: {body}",
        );
    }
}

/// Listing a name that is not a bucket is refused, as the other three routes
/// against that same name already refuse it (Q-261).
///
/// It answered `200` with an empty listing, so a caller concluded the bucket was
/// empty rather than absent — a confident wrong answer, which is the class this
/// store treats as most expensive. The route now asks `INFO FOR BUCKET` first:
/// bucket-ness is settled by a statement through the ordinary session, rather
/// than by a second path into the catalog opened inside the HTTP layer.
#[test]
fn listing_a_table_that_is_not_a_bucket_is_refused_rather_than_answered_empty() {
    let (_node, address) = node();
    assert_eq!(script(&address, READY, None), 200);
    assert_eq!(
        script(
            &address,
            "USE NAMESPACE prod; USE DATABASE library; DEFINE COLLECTION ledger;",
            None
        ),
        200
    );

    let (status, body) = send(&address, "GET", "/files/prod/library/ledger", b"", None);
    let body = String::from_utf8_lossy(&body);
    assert_eq!(
        status, 404,
        "listing a plain table answered as though it were an empty bucket: {body}",
    );
    assert!(
        !body.contains(r#""files":[]"#),
        "the refusal still looks like an empty bucket: {body}",
    );
    assert!(
        body.contains("bucket"),
        "the refusal does not say which part of the URL was missing: {body}",
    );
}

/// The same refusal for a name nothing declared at all.
#[test]
fn listing_a_bucket_nothing_declared_is_refused() {
    let (_node, address) = node();
    assert_eq!(script(&address, READY, None), 200);

    let (status, _) = send(&address, "GET", "/files/prod/library/absent", b"", None);
    assert_eq!(status, 404, "a bucket nobody declared answered a listing");
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
            "USE NAMESPACE prod; USE DATABASE library; DEFINE COLLECTION notes; \
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

#[test]
fn the_write_and_head_methods_refuse_a_caller_without_the_grant_and_serve_one_with_it() {
    // `GET` had a negative test and the other four methods did not, on the
    // argument that they share a code path with something that is tested. That
    // is an argument, and an argument is what a handler added next month
    // silently stops satisfying — so each method is probed here as a path.
    //
    // The probe is the same caller against two buckets, which is what keeps the
    // refusals meaningful: a test where everything is refused would pass just as
    // well against a surface that refuses everybody.
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
            "USE NAMESPACE prod; USE DATABASE library; DEFINE BUCKET archive; \
             DEFINE USER ada ON prod.library ROLE editor PASSWORD 'a long one'; \
             GRANT read, write ON media TO ada;",
            Some(root)
        ),
        200
    );
    // `ada:a long one` in base64. Granted on `media` and not on `archive`.
    let ada = Some("Basic YWRhOmEgbG9uZyBvbmU=");

    let (status, _) = send(
        &address,
        "PUT",
        "/files/prod/library/media/note.txt",
        b"granted",
        ada,
    );
    assert_eq!(status, 201, "the grant did not let a write through");

    for method in ["PUT", "POST"] {
        let (status, _) = send(
            &address,
            method,
            "/files/prod/library/archive/note.txt",
            b"not granted",
            ada,
        );
        assert_eq!(status, 403, "{method} wrote into a bucket nobody granted");
    }

    let (status, _) = send(
        &address,
        "DELETE",
        "/files/prod/library/archive/note.txt",
        b"",
        ada,
    );
    assert_eq!(status, 403, "a delete reached a bucket nobody granted");

    let (status, _) = send(
        &address,
        "HEAD",
        "/files/prod/library/archive/note.txt",
        b"",
        ada,
    );
    assert_eq!(status, 403, "a head reached a bucket nobody granted");

    // And the same method, on the bucket the same caller *was* granted, so the
    // 403s above are about the grant rather than about the method.
    let (status, _) = send(
        &address,
        "HEAD",
        "/files/prod/library/media/note.txt",
        b"",
        ada,
    );
    assert_eq!(status, 200, "the grant did not let a head through");
}

/// All four routes answer `404` for a name that is not a bucket (Q-515).
///
/// # Why `404` and not the `400` this used to be
///
/// `400` is what a **malformed** request gets, and `GET /files/prod/library/ledger`
/// is not malformed — it is a well-formed request for a bucket that is not there.
/// A client told `400` learns that it wrote the request wrongly, which is the one
/// thing it did not do. `respond::failure` has made this correction three times
/// already for the same reason: `NotGranted` answers `403` and not `400`, and
/// `RecordExists` and `StillDepended` answer `409`, each because `400` sends a
/// client to fix the wrong thing.
///
/// The whole `/files` surface now reads one way — **`404` means it is not here**,
/// and the body says which part of "it" was missing. A name declared as a table
/// is `404` rather than `409` because from the caller's side there is no bucket at
/// that URI; the body still carries *"… is not a bucket"*, so nothing that
/// separates the two cases is lost.
#[test]
fn every_files_route_answers_404_for_a_name_that_is_not_a_bucket() {
    let (_node, address) = node();
    assert_eq!(script(&address, READY, None), 200);
    assert_eq!(
        script(
            &address,
            "USE NAMESPACE prod; USE DATABASE library; DEFINE COLLECTION ledger;",
            None
        ),
        200
    );

    // Declared, but as a table; and declared nowhere at all. The listing route
    // cannot tell them apart — `info_bucket` raises one error for both — and the
    // other three reach `Store::bucket`, so this walks the whole surface.
    for bucket in ["ledger", "absent"] {
        for (method, suffix) in [
            ("GET", "/note.txt"),
            ("HEAD", "/note.txt"),
            ("PUT", "/note.txt"),
            ("DELETE", "/note.txt"),
            ("GET", ""),
        ] {
            let path = format!("/files/prod/library/{bucket}{suffix}");
            let (status, body) = send(&address, method, &path, b"held", None);
            assert_eq!(
                status,
                404,
                "{method} {path} answered {status} instead of 404: {}",
                String::from_utf8_lossy(&body),
            );
        }
    }
}

/// The same error over `/script` is still a `400`, and that boundary is the point.
///
/// `Error::Unknown` carries an `entity` and is raised for a table, a namespace, a
/// user and an index as well as for a bucket, so mapping it to `404` inside
/// `respond::failure` would change every route at once. `/script` is an **RPC**
/// surface: the URI is `/script`, the URI is fine, and a `404` there would be a
/// statement about the route rather than about the script. `/files` is a
/// **resource** surface where the URI names the thing that is missing.
///
/// So the mapping lives at the `/files` routes and not in the shared match, and
/// this case is what fails if somebody later moves it.
#[test]
fn the_same_missing_bucket_over_script_is_still_a_bad_request() {
    let (_node, address) = node();
    assert_eq!(script(&address, READY, None), 200);

    let (status, _) = send(
        &address,
        "POST",
        "/script",
        b"USE NAMESPACE prod; USE DATABASE library; INFO FOR BUCKET absent;",
        None,
    );
    assert_eq!(
        status, 400,
        "a statement naming an absent bucket answered 404 — the /files mapping \
         has leaked into the shared failure match, and every route that reports \
         an unknown table, user or index has moved with it",
    );
}

/// The file surface does not delete records out of ordinary tables (Q-515, W206).
///
/// # The defect this pins, which is not about a status code
///
/// `PUT` and `READ` are file statements and resolve the bucket themselves, so
/// they always refused an ordinary table. A delete is not a file statement —
/// there is no `DELETE FILE` — so this route sent a plain record delete, and a
/// plain record delete asks nothing about bucket-ness. `DELETE
/// /files/prod/library/ledger/note.txt` therefore answered **204**: a file
/// removed, from a bucket that does not exist.
///
/// The `204` was the visible half. The expensive half is this test: a record id
/// is arbitrary text, so a path that happens to match one **removed a real
/// record from a real table** through a route that has no business touching
/// records at all. Asserting the status alone would leave that untested, and it
/// is the reason this wave stopped being about a status code.
#[test]
fn a_delete_through_the_file_surface_cannot_reach_a_record_in_a_table() {
    let (_node, address) = node();
    assert_eq!(script(&address, READY, None), 200);
    assert_eq!(
        script(
            &address,
            "USE NAMESPACE prod; USE DATABASE library; DEFINE COLLECTION ledger; \
             CREATE ledger:'/note.txt' = { amount: 1 };",
            None
        ),
        200
    );

    let (status, body) = send(
        &address,
        "DELETE",
        "/files/prod/library/ledger/note.txt",
        b"",
        None,
    );
    assert_eq!(
        status,
        404,
        "a delete against a table answered {status}: {}",
        String::from_utf8_lossy(&body),
    );

    // The status is not the assertion that matters. This is.
    let (status, body) = send(
        &address,
        "POST",
        "/script",
        b"USE NAMESPACE prod; USE DATABASE library; SELECT amount FROM ledger;",
        None,
    );
    let body = String::from_utf8_lossy(&body);
    assert_eq!(status, 200, "{body}");
    assert!(
        body.contains("amount"),
        "the file route deleted a record out of an ordinary table: {body}",
    );
}

/// A missing namespace is not a missing bucket, and `/files` says so (Q-515).
///
/// The `404` mapping names `"bucket"` and `"table"` because on this surface both
/// can only mean the bucket segment. A namespace and a database are their own
/// entities and are deliberately left where they were: this wave answered the
/// question the owner asked — *what does "it is not a bucket" answer* — and
/// widening to the other two segments is a larger decision than that one
/// (Q-517). The published specification says the same in the same words: it
/// specifies the bucket segment only.
///
/// Without this case the restriction inside the mapping is documentation. A
/// `matches!` widened to every `Error::Unknown` passes the whole suite otherwise,
/// which is how a narrowing survives in a comment and dies in the code.
#[test]
fn a_missing_namespace_on_the_file_surface_is_not_reported_as_a_missing_bucket() {
    let (_node, address) = node();
    assert_eq!(script(&address, READY, None), 200);

    let (status, body) = send(&address, "GET", "/files/nope/library/media", b"", None);
    assert_eq!(
        status,
        400,
        "a missing namespace answered {status}: the /files 404 mapping has \
         widened past the bucket segment, which this version does not specify: {}",
        String::from_utf8_lossy(&body),
    );
}
