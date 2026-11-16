//! The TLS rows: SSLRequest answered `S` over a certificate and `N` without
//! one, and the startup inside the negotiated channel.

use std::sync::Arc;

use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::TcpStream;

use super::{Client, SETUP, Seen, rows, serving_adjusted};

#[tokio::test(flavor = "multi_thread")]
async fn an_ssl_request_is_answered_n_without_a_certificate_and_the_session_runs() {
    let (address, stop, _) = serving_adjusted(SETUP, |node| node).await;
    let mut client = Client::to(&address).await;
    // SSLRequest: length 8, the request code.
    let mut ask = Vec::new();
    ask.extend_from_slice(&8_u32.to_be_bytes());
    ask.extend_from_slice(&crate::codec::SSL_REQUEST.to_be_bytes());
    client.write(&ask).await;
    let answer = client.read(1).await;
    assert_eq!(answer[0], b'N', "no certificate: plaintext continues");
    let seen = client.start(None, "app.main", None).await;
    assert!(seen.contains(&Seen::Auth(0)), "{seen:?}");
    let seen = client.query("SELECT id FROM orders LIMIT 1").await;
    assert_eq!(rows(&seen).len(), 1);
    stop.cancel();
}

#[tokio::test(flavor = "multi_thread")]
async fn an_ssl_request_is_answered_s_over_a_certificate_and_tls_carries_the_session() {
    let (server, client_config) = tessari_serve::testing::loopback().expect("a test credential");
    let (address, stop, _) = serving_adjusted(SETUP, |node| node.securing(server)).await;
    let connector = tokio_rustls::TlsConnector::from(Arc::clone(&client_config));
    let name = rustls::pki_types::ServerName::try_from("127.0.0.1").expect("a server name");
    // The dialect's own shape: SSLRequest in the clear, the S, then the TLS
    // handshake over the same connection, and the startup inside it.
    let mut plain = TcpStream::connect(&address)
        .await
        .expect("the node answers");
    let mut ask = Vec::new();
    ask.extend_from_slice(&8_u32.to_be_bytes());
    ask.extend_from_slice(&crate::codec::SSL_REQUEST.to_be_bytes());
    plain.writable().await.expect("the socket is ready");
    plain.try_write(&ask).expect("the request sends");
    let mut answer = [0_u8; 1];
    plain.read_exact(&mut answer).await.expect("the S arrives");
    assert_eq!(answer[0], b'S');
    let mut stream = connector
        .connect(name, plain)
        .await
        .expect("the handshake passes");
    // The startup runs inside the negotiated channel and the session greets:
    // AuthenticationOk is the first frame back on an open store.
    let mut startup = Vec::new();
    let body = {
        let mut inner = Vec::new();
        inner.extend_from_slice(&crate::codec::PROTOCOL_VERSION.to_be_bytes());
        for (key, value) in [("user", ""), ("database", "app.main")] {
            if key == "user" {
                continue;
            }
            inner.extend_from_slice(key.as_bytes());
            inner.push(0);
            inner.extend_from_slice(value.as_bytes());
            inner.push(0);
        }
        inner.push(0);
        inner
    };
    startup.extend_from_slice(
        &u32::try_from(body.len().saturating_add(4))
            .expect("small")
            .to_be_bytes(),
    );
    startup.extend_from_slice(&body);
    stream.write_all(&startup).await.expect("the startup sends");
    let mut kind = [0_u8; 1];
    stream
        .read_exact(&mut kind)
        .await
        .expect("the greeting arrives");
    assert_eq!(kind[0], b'R', "AuthenticationOk opens the session");
    stop.cancel();
}
