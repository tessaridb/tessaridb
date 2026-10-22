//! The negative rows for this surface (G079 C8): a node with a certificate
//! speaks TLS and nothing else, and a signed-in user reaches streams exactly
//! as far as the store's grants say.

use std::sync::Arc;
use std::time::Duration;

use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::TcpStream;

use super::{Client, error_starting, parse, serving_adjusted, serving_with};
use crate::codec::Reply;

/// `PING` over `stream`, and what came back before it closed or answered.
async fn ping<S>(mut stream: S) -> Vec<u8>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    if stream.write_all(b"*1\r\n$4\r\nPING\r\n").await.is_err() {
        return Vec::new();
    }
    let mut back = Vec::new();
    let mut chunk = [0_u8; 512];
    while parse(&back).is_none() {
        match tokio::time::timeout(Duration::from_secs(5), stream.read(&mut chunk)).await {
            Ok(Ok(read)) if read > 0 => back.extend_from_slice(&chunk[..read]),
            _ => break,
        }
    }
    back
}

#[tokio::test(flavor = "multi_thread")]
async fn a_node_with_a_certificate_answers_tls_and_nothing_in_the_clear() {
    let (server, client) = tessari_serve::testing::loopback().unwrap();
    let (address, stop, _) = serving_adjusted("", |node| node.securing(server)).await;
    let connector = tokio_rustls::TlsConnector::from(Arc::clone(&client));
    let name = rustls::pki_types::ServerName::try_from("127.0.0.1").unwrap();
    let secured = connector
        .connect(name, TcpStream::connect(&address).await.unwrap())
        .await
        .unwrap();
    assert_eq!(ping(secured).await, b"+PONG\r\n");
    // In the clear the bytes are a failed handshake: no reply is ever read.
    let answered = ping(TcpStream::connect(&address).await.unwrap()).await;
    assert!(
        !answered.windows(4).any(|window| window == b"PONG"),
        "{answered:?}"
    );
    stop.cancel();
}

#[tokio::test(flavor = "multi_thread")]
async fn a_viewer_reads_a_stream_and_is_refused_every_write_to_it() {
    let (address, stop, db) =
        serving_with("DEFINE USER root ROLE owner PASSWORD 'secret-one';").await;
    let mut owner = db.session();
    owner.sign_in("root", "secret-one").await.unwrap();
    owner
        .run("DEFINE USER reader ON app.main ROLE viewer PASSWORD 'secret-two';")
        .await
        .unwrap();
    drop(owner);
    let mut root = Client::to(&address).await;
    root.ask(&["AUTH", "root", "secret-one"]).await;
    root.ask(&["XADD", "audit", "*", "n", "1"]).await;
    root.ask(&["XGROUP", "CREATE", "audit", "g", "0"]).await;
    let mut reader = Client::to(&address).await;
    assert!(error_starting(
        &reader.ask(&["XRANGE", "audit", "-", "+"]).await,
        "NOAUTH"
    ));
    assert_eq!(
        reader.ask(&["AUTH", "reader", "secret-two"]).await,
        Reply::OK
    );
    assert_eq!(reader.ask(&["XLEN", "audit"]).await, Reply::Integer(1));
    let ranged = reader.ask(&["XRANGE", "audit", "-", "+"]).await;
    assert!(
        matches!(&ranged, Reply::Array(entries) if entries.len() == 1),
        "{ranged:?}"
    );
    for refused in [
        &["XADD", "audit", "*", "n", "2"][..],
        &["XADD", "fresh", "*", "n", "1"],
        &["XTRIM", "audit", "MAXLEN", "0"],
        &["XGROUP", "CREATE", "audit", "mine", "0"],
        &["XGROUP", "DESTROY", "audit", "g"],
    ] {
        let reply = reader.ask(refused).await;
        assert!(error_starting(&reply, "NOPERM"), "{refused:?}: {reply:?}");
    }
    // Nothing the refused writes asked for happened.
    assert_eq!(root.ask(&["XLEN", "audit"]).await, Reply::Integer(1));
    assert_eq!(root.ask(&["XLEN", "fresh"]).await, Reply::Integer(0));
    let groups = root.ask(&["XINFO", "GROUPS", "audit"]).await;
    assert!(
        matches!(&groups, Reply::Array(groups) if groups.len() == 1),
        "the refused DESTROY left the group: {groups:?}"
    );
    // Consuming under a group is reading the topic in this store, as a Kafka
    // consumer's is: a viewer reads and settles, and holds what it read.
    let consumed = reader
        .ask(&["XREADGROUP", "GROUP", "g", "c", "STREAMS", "audit", ">"])
        .await;
    assert!(
        matches!(&consumed, Reply::Array(streams) if streams.len() == 1),
        "{consumed:?}"
    );
    let pending = root.ask(&["XPENDING", "audit", "g"]).await;
    assert!(
        matches!(&pending, Reply::Array(parts) if parts.first() == Some(&Reply::Integer(1))),
        "{pending:?}"
    );
    stop.cancel();
}
