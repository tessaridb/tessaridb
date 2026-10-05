//! The HTTP surface over TLS (ADR-0108 D4): answered to a client that verified
//! the node, and never in the clear.

#![allow(clippy::unwrap_used, clippy::indexing_slicing)]

use std::io::{Read, Write};
use std::net::TcpStream;
use std::sync::Arc;

use tessari_http::Node;
use tessari_serve::tls::{self, Pem};
use tessaridb::Db;

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

/// An HTTP node speaking TLS, carrying a wire node's door, and the authority
/// that issued its certificate.
fn served() -> (String, String) {
    let (chain, key, authority) = issued_for_loopback();
    let settings = tls::Credential::read(
        Pem {
            bytes: chain.as_bytes(),
            path: "cert.pem",
        },
        Pem {
            bytes: key.as_bytes(),
            path: "key.pem",
        },
    )
    .unwrap()
    .server_config(&[b"http/1.1"]);
    let db = Arc::new(Db::in_memory().unwrap());
    let wire = tessari_wire::Node::bind(Arc::clone(&db), "127.0.0.1:0").unwrap();
    let mut http = Node::bind(db, "127.0.0.1:0").unwrap();
    http.carrying(crate::wire_socket::door_of(&wire));
    http.securing(settings);
    let http = Arc::new(http);
    let address = http.address();
    std::thread::spawn(move || crate::serve_until_the_test_ends(&http));
    (address, authority)
}

/// Send `head` over TLS trusting `authority`, and read what comes back.
fn over_tls(address: &str, authority: &str, head: &str) -> String {
    let roots = tls::authority(Pem {
        bytes: authority.as_bytes(),
        path: "ca.pem",
    })
    .unwrap();
    let settings = rustls::ClientConfig::builder()
        .with_root_certificates(roots)
        .with_no_client_auth();
    let name = rustls::pki_types::ServerName::try_from("127.0.0.1").unwrap();
    let session = rustls::ClientConnection::new(Arc::new(settings), name).unwrap();
    let mut stream = rustls::StreamOwned::new(session, TcpStream::connect(address).unwrap());
    stream.write_all(head.as_bytes()).unwrap();
    stream.flush().unwrap();
    let mut back = Vec::new();
    // Read what arrives; an upgraded socket stays open, so stop at the head.
    let mut chunk = [0_u8; 4096];
    loop {
        match stream.read(&mut chunk) {
            Ok(0) | Err(_) => break,
            Ok(read) => {
                back.extend_from_slice(&chunk[..read]);
                if head.contains("Upgrade") && back.windows(4).any(|w| w == b"\r\n\r\n") {
                    break;
                }
            }
        }
    }
    String::from_utf8_lossy(&back).into_owned()
}

#[test]
fn a_node_with_a_certificate_answers_routes_and_scripts_over_tls() {
    let (address, authority) = served();
    let health = over_tls(
        &address,
        &authority,
        "GET /health HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n",
    );
    assert!(health.starts_with("HTTP/1.1 200"), "{health}");

    let script = "RETURN 40 + 2;";
    let answered = over_tls(
        &address,
        &authority,
        &format!(
            "POST /script HTTP/1.1\r\nHost: 127.0.0.1\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{script}",
            script.len()
        ),
    );
    assert!(answered.starts_with("HTTP/1.1 200"), "{answered}");
    assert!(answered.contains("42"), "{answered}");
}

#[test]
fn the_wire_socket_rides_the_same_tls() {
    let (address, authority) = served();
    let upgraded = over_tls(
        &address,
        &authority,
        "GET /wire HTTP/1.1\r\nHost: 127.0.0.1\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n\
         Sec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\nSec-WebSocket-Version: 13\r\n\r\n",
    );
    assert!(upgraded.starts_with("HTTP/1.1 101"), "{upgraded}");
}

#[test]
fn a_node_with_a_certificate_answers_no_request_in_the_clear() {
    let (address, _) = served();
    let mut stream = TcpStream::connect(&address).unwrap();
    stream
        .write_all(b"GET /health HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n")
        .unwrap();
    let mut back = Vec::new();
    drop(stream.read_to_end(&mut back));
    assert!(
        !back.starts_with(b"HTTP/"),
        "the node answered HTTP in the clear: {}",
        String::from_utf8_lossy(&back)
    );
}

#[test]
fn a_node_serving_tls_tells_a_browser_to_keep_using_it() {
    // The half `console.rs` cannot ask: in the clear the header is absent, and
    // over TLS it is present, so a browser that reached the console once over
    // TLS never falls back to sending its token in the clear.
    let (address, authority) = served();
    let answered = over_tls(
        &address,
        &authority,
        "GET /health HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n",
    );
    let head = answered
        .split("\r\n\r\n")
        .next()
        .unwrap_or_default()
        .to_ascii_lowercase();
    assert!(
        head.contains("strict-transport-security: max-age=63072000"),
        "{answered}"
    );
}
