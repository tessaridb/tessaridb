//! What an HTTP caller can and cannot be told about a vault.
//!
//! Survey row 19 — the response encoder — was dispositioned `tested` in a wave
//! that never ran (Q-424). This is that test, and it is written the way the CLI
//! one is: the real encoder, over a real socket, with a planted secret and a
//! control, one request per probe.
//!
//! # The request path is a surface of its own
//!
//! Criterion I1 names four places a secret must never be written, and one of
//! them is a **URL**. A URL is worse than a body: it is logged by every proxy in
//! the path, kept in browser history, and sent in a `Referer`. This interface
//! takes its script in the **body** of a `POST`, so the secret has no reason to
//! be in a path — and the assertion below says so rather than leaving it to be
//! noticed if somebody adds a `GET /reveal/{record}` next month.

#![allow(clippy::panic, clippy::unwrap_used, clippy::indexing_slicing)]

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::sync::Arc;

use tessari_http::Node;
use tessaridb::Db;

const PLANTED: &str = "correct-horse-battery-staple-9f2b";
const PASSPHRASE: &str = "an operator passphrase 4b71";
const CONTROL: &str = "ada-lovelace-marker-71c4";

fn node() -> (Arc<Node>, String) {
    let db = Arc::new(Db::in_memory().unwrap());
    let node = Arc::new(Node::bind(db, "127.0.0.1:0").unwrap());
    let address = node.address();
    let serving = Arc::clone(&node);
    std::thread::spawn(move || serving.serve());
    (node, address)
}

/// One request, and everything that came back — status line, headers and body
/// together, because a header is as public as a body and a test reading only the
/// body would be blind to half of what was sent.
fn ask(address: &str, path: &str, body: &str) -> (u16, String) {
    let mut stream = TcpStream::connect(address).unwrap();
    let head = format!(
        "POST {path} HTTP/1.1\r\nHost: {address}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
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

    let mut said = status_line.clone();
    loop {
        let mut line = String::new();
        reader.read_line(&mut line).unwrap();
        said.push_str(&line);
        if line.trim().is_empty() {
            break;
        }
    }
    let mut answered = String::new();
    reader.read_to_string(&mut answered).unwrap();
    said.push_str(&answered);
    (status, said)
}

const TENANCY: &str = "USE NAMESPACE prod; USE DATABASE work;";

/// The vault, its record, and an ordinary table holding the control.
fn ready(address: &str) {
    let (status, said) = ask(
        address,
        "/script",
        &format!(
            "DEFINE NAMESPACE prod; USE NAMESPACE prod;
             DEFINE DATABASE work; USE DATABASE work;
             UNSEAL VAULT WITH '{PASSPHRASE}';
             DEFINE VAULT team;
             DEFINE FIELD login ON team TYPE string;
             DEFINE FIELD token ON team TYPE string SECRET;
             CREATE team:'github' = {{ login: '{CONTROL}', token: '{PLANTED}' }};
             DEFINE TABLE notes SCHEMALESS;
             CREATE notes:1 = {{ text: '{CONTROL}' }};"
        ),
    );
    assert_eq!(status, 200, "{said}");
}

#[test]
fn only_a_reveal_puts_a_secret_in_an_http_response() {
    let (_node, address) = node();
    ready(&address);

    // The control: this encoder does render a value when a statement answers
    // with one, so a silent encoder cannot make the scans below pass.
    let (status, said) = ask(
        &address,
        "/script",
        &format!("{TENANCY} SELECT * FROM notes;"),
    );
    assert_eq!(status, 200, "{said}");
    assert!(
        said.contains(CONTROL),
        "the encoder rendered no value: {said}"
    );

    // The one statement allowed to answer with a plaintext, asserted before the
    // absences — otherwise a store that never held the secret would pass them
    // all.
    let (_, said) = ask(
        &address,
        "/script",
        &format!("{TENANCY} REVEAL token FROM team:'github';"),
    );
    assert!(
        said.contains(PLANTED),
        "`REVEAL` returned no secret: {said}"
    );

    for probe in [
        "SELECT * FROM team;",
        "SELECT * FROM team:'github';",
        "SELECT * FROM team WHERE token = 'guess';",
        "REVEAL login FROM team:'github';",
        "INFO FOR VAULT team;",
        "INFO FOR TABLE team;",
        "INFO FOR RECIPIENTS OF team:'github';",
        "DEFINE INDEX by_token ON team FIELDS token;",
        "ALTER TABLE team SET SCHEMALESS;",
        "CREATE team:'gitlab' = { login: 'boog', recovery: 'anything' };",
    ] {
        let (_, said) = ask(&address, "/script", &format!("{TENANCY} {probe}"));
        assert!(
            !said.contains(PLANTED),
            "`{probe}` put a secret in an HTTP response: {said}"
        );
        assert!(
            !said.contains(PASSPHRASE),
            "`{probe}` echoed the unseal passphrase: {said}"
        );
    }
}

/// I1's URL clause, asserted rather than assumed.
///
/// A secret in a path is logged by every proxy between here and the caller, and
/// this interface gives it no reason to be there: the script travels in the
/// body. The probe sends a path that *does* carry a secret and asserts the node
/// refuses it rather than routing on it — so a future route that reads a record
/// out of its URL fails this test on the way in.
#[test]
fn a_secret_has_no_place_in_a_request_path() {
    let (_node, address) = node();
    ready(&address);

    let (status, said) = ask(&address, &format!("/script/{PLANTED}"), "");
    assert_ne!(status, 200, "a path carrying a secret was served: {said}");

    // And what it said about refusing does not repeat the path back. An echoed
    // URL in a 404 body is the same disclosure as a served one, reached by a
    // different route.
    assert!(
        !said.contains(PLANTED),
        "the refusal echoed the secret it was sent in the path: {said}"
    );
}
