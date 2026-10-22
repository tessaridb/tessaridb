//! The negative rows for this surface (G079 C8): a node with a certificate
//! speaks TLS and nothing else, a refused sign-in is answered and the
//! connection closed, and a signed-in user produces and creates only as far
//! as the store's grants say.

use std::sync::Arc;
use std::time::Duration;

use tokio::io::AsyncReadExt as _;
use tokio::net::TcpStream;

use super::admin::{create, sign_in};
use super::{ask, fetch_body, fetched, produce_body, produced, serving_adjusted, serving_with};
use crate::codec::{Reader, Writer};
use crate::records::{Outgoing, encode_batch};

/// `ApiVersions` v0 over `stream`: its error code.
async fn versions<S>(stream: &mut S) -> i16
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    let answer = ask(stream, 18, 0, false, &[]).await;
    Reader::new(&answer).i16("error").unwrap()
}

/// What the node sends back before `stream` closes or a second passes.
async fn what_comes_back(stream: &mut TcpStream) -> Vec<u8> {
    let mut back = Vec::new();
    let mut chunk = [0_u8; 64];
    while let Ok(Ok(read)) =
        tokio::time::timeout(Duration::from_secs(1), stream.read(&mut chunk)).await
    {
        if read == 0 {
            break;
        }
        back.extend_from_slice(&chunk[..read]);
    }
    back
}

/// An `ApiVersions` v0 request with correlation id 7, framed.
fn versions_frame() -> Vec<u8> {
    let mut frame = Writer::new();
    frame.i16(18);
    frame.i16(0);
    frame.i32(7);
    frame.nullable_string(false, Some("test")).unwrap();
    let frame = frame.into_bytes();
    let mut sent = u32::try_from(frame.len()).unwrap().to_be_bytes().to_vec();
    sent.extend_from_slice(&frame);
    sent
}

fn one_record() -> Vec<u8> {
    let mut batch = Writer::new();
    encode_batch(
        &mut batch,
        0,
        &[Outgoing {
            timestamp: 1_760_000_000_000,
            key: None,
            value: Some(b"refused"),
            headers: &[],
        }],
    )
    .unwrap();
    batch.into_bytes()
}

#[tokio::test(flavor = "multi_thread")]
async fn a_node_with_a_certificate_answers_tls_and_nothing_in_the_clear() {
    let (server, client) = tessari_serve::testing::loopback().unwrap();
    let (address, stop, _) = serving_adjusted("", |node| node.securing(server)).await;
    let connector = tokio_rustls::TlsConnector::from(Arc::clone(&client));
    let name = rustls::pki_types::ServerName::try_from("127.0.0.1").unwrap();
    let mut secured = connector
        .connect(name, TcpStream::connect(&address).await.unwrap())
        .await
        .unwrap();
    assert_eq!(versions(&mut secured).await, 0);
    // In the clear a request is a failed handshake: what comes back is a TLS
    // alert record (content type 21) or nothing, never a Kafka answer.
    let mut clear = TcpStream::connect(&address).await.unwrap();
    tokio::io::AsyncWriteExt::write_all(&mut clear, &versions_frame())
        .await
        .unwrap();
    let back = what_comes_back(&mut clear).await;
    assert!(back.is_empty() || back.first() == Some(&0x15), "{back:?}");
    stop.cancel();
}

#[tokio::test(flavor = "multi_thread")]
async fn a_refused_sign_in_is_answered_and_the_connection_closed() {
    let (address, stop, _) =
        serving_with("DEFINE USER root ROLE owner PASSWORD 'secret-one';").await;
    let mut stream = TcpStream::connect(&address).await.unwrap();
    let mut body = Writer::new();
    body.string(false, "PLAIN").unwrap();
    let answer = ask(&mut stream, 17, 1, false, &body.into_bytes()).await;
    assert_eq!(Reader::new(&answer).i16("error").unwrap(), 0);
    let token = b"\0root\0wrong".to_vec();
    let mut body = Writer::new();
    body.bytes(true, &token).unwrap();
    body.no_tagged_fields();
    let answer = ask(&mut stream, 36, 2, true, &body.into_bytes()).await;
    assert_eq!(
        Reader::new(&answer).i16("error").unwrap(),
        crate::api::sasl::SASL_AUTHENTICATION_FAILED
    );
    let mut chunk = [0_u8; 8];
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(5), stream.read(&mut chunk))
            .await
            .unwrap()
            .unwrap(),
        0,
        "nothing else may be sent on it"
    );
    // Unsigned, a closed store answers no topic.
    let mut anonymous = TcpStream::connect(&address).await.unwrap();
    assert_eq!(
        produced(
            &ask(
                &mut anonymous,
                0,
                9,
                true,
                &produce_body("orders", &one_record())
            )
            .await
        )
        .0,
        crate::surface::errors::TOPIC_AUTHORIZATION_FAILED
    );
    stop.cancel();
}

#[tokio::test(flavor = "multi_thread")]
async fn a_viewer_fetches_and_is_refused_every_produce_and_create() {
    let (address, stop, db) =
        serving_with("DEFINE USER root ROLE owner PASSWORD 'secret-one';").await;
    let mut owner = db.session();
    owner.sign_in("root", "secret-one").await.unwrap();
    owner
        .run(
            "DEFINE USER reader ON app.main ROLE viewer PASSWORD 'secret-two'; \
             USE NAMESPACE app; USE DATABASE main; CREATE orders = { n: 1 };",
        )
        .await
        .unwrap();
    drop(owner);
    let mut reader = TcpStream::connect(&address).await.unwrap();
    sign_in(&mut reader, "reader", "secret-two").await;
    let (code, _, records) =
        fetched(&ask(&mut reader, 1, 12, true, &fetch_body("orders", 0)).await);
    assert_eq!(code, 0);
    assert!(!records.is_empty(), "a viewer reads what is there");
    assert_eq!(
        produced(
            &ask(
                &mut reader,
                0,
                9,
                true,
                &produce_body("orders", &one_record())
            )
            .await
        )
        .0,
        crate::surface::errors::TOPIC_AUTHORIZATION_FAILED
    );
    assert_ne!(
        create(&mut reader, "made_by_a_viewer", 1, &[], false).await,
        0
    );
    // Nothing the refused requests asked for happened.
    let mut check = db.session();
    check.sign_in("root", "secret-one").await.unwrap();
    let held = check
        .run("USE NAMESPACE app; USE DATABASE main; READ FROM orders AFTER 0 LIMIT 10;")
        .await
        .unwrap();
    assert!(
        matches!(held.last(), Some(tessaridb::Outcome::Records { records, .. }) if records.len() == 1),
        "only the owner's message is on the topic: {held:?}"
    );
    assert!(
        check
            .run("USE NAMESPACE app; USE DATABASE main; INFO FOR TOPIC made_by_a_viewer;")
            .await
            .is_err()
    );
    stop.cancel();
}

#[tokio::test(flavor = "multi_thread")]
async fn a_full_door_turns_the_next_connection_away_and_a_freed_place_admits_it() {
    let mut door = None;
    let (address, stop, _) = serving_adjusted("", |node| {
        door = Some(Arc::clone(&node.door));
        node
    })
    .await;
    let door = door.unwrap();
    // Every place taken directly, rather than by opening that many connections.
    let mut places = Vec::new();
    while let Some(place) = door.admit() {
        places.push(place);
    }
    let mut turned = TcpStream::connect(&address).await.unwrap();
    drop(tokio::io::AsyncWriteExt::write_all(&mut turned, &versions_frame()).await);
    let back = what_comes_back(&mut turned).await;
    assert!(
        back.is_empty(),
        "a turned-away connection was answered: {back:?}"
    );
    drop(places);
    let mut admitted = TcpStream::connect(&address).await.unwrap();
    assert_eq!(versions(&mut admitted).await, 0);
    stop.cancel();
}

/// Read until the node closes `stream`, and say how long that took on the
/// test's clock.
async fn closed_after<S: tokio::io::AsyncRead + Unpin>(stream: &mut S) -> Duration {
    let began = tokio::time::Instant::now();
    let mut chunk = [0_u8; 64];
    while stream.read(&mut chunk).await.unwrap_or(0) > 0 {}
    began.elapsed()
}

#[tokio::test(start_paused = true)]
async fn a_connection_that_says_nothing_is_closed_when_its_idle_time_runs_out() {
    let (address, stop, _) = serving_with("").await;
    let mut idle = TcpStream::connect(&address).await.unwrap();
    let waited = closed_after(&mut idle).await;
    assert!(
        waited >= Duration::from_secs(tessari_constants::KAFKA_IDLE_SECONDS),
        "{waited:?}"
    );
    stop.cancel();
}

#[tokio::test(start_paused = true)]
async fn a_tls_connection_that_never_greets_is_closed_at_the_greeting_limit() {
    let (server, _) = tessari_serve::testing::loopback().unwrap();
    let (address, stop, _) = serving_adjusted("", |node| node.securing(server)).await;
    let mut silent = TcpStream::connect(&address).await.unwrap();
    let waited = closed_after(&mut silent).await;
    assert!(
        waited >= Duration::from_secs(tessari_constants::GREETING_SECONDS)
            && waited < Duration::from_secs(tessari_constants::KAFKA_IDLE_SECONDS),
        "{waited:?}"
    );
    stop.cancel();
}
