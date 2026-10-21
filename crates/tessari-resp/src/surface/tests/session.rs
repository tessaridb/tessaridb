//! The connection: pipelining, transactions, protocol versions, identity.

use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

use super::{Client, bulk, error_starting, serving_with, status};
use crate::codec::Reply;

#[tokio::test(flavor = "multi_thread")]
async fn a_pipeline_is_answered_in_order() {
    let (address, stop, _) = serving_with("").await;
    let mut client = Client::to(&address).await;
    let mut pipeline = Vec::new();
    for _ in 0..2_000 {
        pipeline.extend_from_slice(b"*2\r\n$4\r\nINCR\r\n$1\r\nn\r\n");
    }
    pipeline.extend_from_slice(b"PING\r\n");
    client.stream.write_all(&pipeline).await.unwrap();
    for expected in 1..=2_000 {
        assert_eq!(client.reply().await, Reply::Integer(expected));
    }
    assert_eq!(client.reply().await, status("PONG"));
    stop.cancel();
}

#[tokio::test(flavor = "multi_thread")]
async fn exec_runs_the_queue_as_one_transaction() {
    let (address, stop, _) = serving_with("").await;
    let mut client = Client::to(&address).await;
    assert_eq!(client.ask(&["MULTI"]).await, Reply::OK);
    assert_eq!(client.ask(&["SET", "a", "1"]).await, status("QUEUED"));
    assert_eq!(client.ask(&["INCR", "a"]).await, status("QUEUED"));
    assert_eq!(client.ask(&["GET", "a"]).await, status("QUEUED"));
    assert_eq!(client.ask(&["PING"]).await, status("QUEUED"));
    assert_eq!(
        client.ask(&["EXEC"]).await,
        Reply::Array(vec![
            Reply::OK,
            Reply::Integer(2),
            bulk("2"),
            status("PONG")
        ])
    );
    // A command that cannot be queued discards the whole transaction.
    client.ask(&["MULTI"]).await;
    client.ask(&["SET", "b", "1"]).await;
    assert!(error_starting(
        &client.ask(&["SET"]).await,
        "ERR wrong number"
    ));
    assert!(error_starting(&client.ask(&["EXEC"]).await, "EXECABORT"));
    assert_eq!(client.ask(&["GET", "b"]).await, Reply::Nil);
    // A command the store refuses refuses the whole EXEC: nothing is written.
    client.ask(&["SET", "word", "abc"]).await;
    client.ask(&["MULTI"]).await;
    client.ask(&["SET", "c", "1"]).await;
    client.ask(&["INCR", "word"]).await;
    assert!(error_starting(&client.ask(&["EXEC"]).await, "EXECABORT"));
    assert_eq!(client.ask(&["GET", "c"]).await, Reply::Nil);
    client.ask(&["MULTI"]).await;
    client.ask(&["SET", "d", "1"]).await;
    assert_eq!(client.ask(&["DISCARD"]).await, Reply::OK);
    assert_eq!(client.ask(&["GET", "d"]).await, Reply::Nil);
    assert_eq!(
        client.ask(&["EXEC"]).await,
        Reply::error("ERR EXEC without MULTI")
    );
    stop.cancel();
}

#[tokio::test(flavor = "multi_thread")]
async fn hello_switches_the_protocol_and_names_the_server() {
    let (address, stop, _) = serving_with("").await;
    let mut client = Client::to(&address).await;
    let hello = client.ask(&["HELLO", "3", "SETNAME", "app"]).await;
    let fields = match hello {
        Reply::Map(fields) => fields,
        _ => Vec::new(),
    };
    assert!(fields.contains(&(bulk("server"), bulk("tessaridb"))));
    assert!(fields.contains(&(bulk("proto"), Reply::Integer(3))));
    // RESP3 answers a missing key with its own null.
    client.send(&[b"GET", b"missing"]).await;
    let mut raw = [0_u8; 3];
    client.stream.read_exact(&mut raw).await.unwrap();
    assert_eq!(&raw, b"_\r\n");
    assert_eq!(client.ask(&["CLIENT", "GETNAME"]).await, bulk("app"));
    assert_eq!(
        client.ask(&["HELLO", "4"]).await,
        Reply::error("NOPROTO unsupported protocol version")
    );
    assert_eq!(client.ask(&["SELECT", "0"]).await, Reply::OK);
    assert!(error_starting(
        &client.ask(&["SELECT", "1"]).await,
        "ERR DB index"
    ));
    assert!(error_starting(
        &client.ask(&["NOSUCH"]).await,
        "ERR unknown command"
    ));
    stop.cancel();
}

#[tokio::test(flavor = "multi_thread")]
async fn a_closed_store_answers_only_a_signed_in_user() {
    let (address, stop, db) =
        serving_with("DEFINE USER root ROLE owner PASSWORD 'secret-one';").await;
    // Only an owner declares a user once the store is closed.
    let mut owner = db.session();
    owner.sign_in("root", "secret-one").await.unwrap();
    owner
        .run("DEFINE USER reader ON app.main ROLE viewer PASSWORD 'secret-two';")
        .await
        .unwrap();
    drop(owner);
    let mut client = Client::to(&address).await;
    assert!(error_starting(&client.ask(&["GET", "k"]).await, "NOAUTH"));
    assert_eq!(
        client.ask(&["AUTH", "root", "wrong"]).await,
        Reply::error("WRONGPASS invalid username-password pair or user is disabled.")
    );
    assert_eq!(client.ask(&["AUTH", "root", "secret-one"]).await, Reply::OK);
    assert_eq!(client.ask(&["SET", "k", "v"]).await, Reply::OK);
    let mut reader = Client::to(&address).await;
    let hello = reader
        .ask(&["HELLO", "2", "AUTH", "reader", "secret-two"])
        .await;
    // RESP2 writes HELLO's map as a flat array of pairs.
    assert!(
        matches!(&hello, Reply::Array(items) if items.first() == Some(&bulk("server"))),
        "{hello:?}"
    );
    assert_eq!(reader.ask(&["GET", "k"]).await, bulk("v"));
    assert!(error_starting(
        &reader.ask(&["SET", "k", "w"]).await,
        "NOPERM"
    ));
    // Wrong passwords reach the store's throttle, which says wait rather than
    // "wrong password": signing in again with the right one is what helps.
    let mut throttled = None;
    for _ in 0..tessari_constants::FREE_SIGN_IN_FAILURES.saturating_add(2) {
        let reply = client.ask(&["AUTH", "reader", "wrong"]).await;
        if !error_starting(&reply, "WRONGPASS") {
            throttled = Some(reply);
            break;
        }
    }
    assert!(
        throttled.is_some_and(|reply| error_starting(&reply, "TRYAGAIN")),
        "the throttle is answered as try again"
    );
    stop.cancel();
}

#[tokio::test(flavor = "multi_thread")]
async fn a_connection_names_its_own_space_and_unreadable_bytes_close_it() {
    let (address, stop, _) = serving_with("DEFINE SPACE other;").await;
    let mut client = Client::to(&address).await;
    client.ask(&["SET", "k", "in-cache"]).await;
    assert_eq!(
        client.ask(&["TESSARIDB.USE", "app", "main", "other"]).await,
        Reply::OK
    );
    assert_eq!(client.ask(&["GET", "k"]).await, Reply::Nil);
    assert!(error_starting(
        &client
            .ask(&["TESSARIDB.USE", "app", "main", "bad-name"])
            .await,
        "ERR"
    ));
    client.stream.write_all(b"*1\r\n:5\r\n").await.unwrap();
    assert!(error_starting(&client.reply().await, "ERR Protocol error"));
    let mut rest = Vec::new();
    client.stream.read_to_end(&mut rest).await.unwrap();
    assert!(rest.is_empty(), "the node closed the connection");
    stop.cancel();
}

#[tokio::test(flavor = "multi_thread")]
async fn a_space_held_in_memory_is_served_as_any_space() {
    let (address, stop, _) = serving_with("DEFINE SPACE hot IN MEMORY MAX 1000;").await;
    let mut client = Client::to(&address).await;
    assert_eq!(
        client.ask(&["TESSARIDB.USE", "app", "main", "hot"]).await,
        Reply::OK
    );
    assert_eq!(client.ask(&["SET", "n", "1", "EX", "60"]).await, Reply::OK);
    assert_eq!(client.ask(&["INCR", "n"]).await, Reply::Integer(2));
    assert!(matches!(client.ask(&["TTL", "n"]).await, Reply::Integer(left) if left > 0));
    client.ask(&["MULTI"]).await;
    client.ask(&["INCR", "n"]).await;
    client.ask(&["SET", "m", "x"]).await;
    assert_eq!(
        client.ask(&["EXEC"]).await,
        Reply::Array(vec![Reply::Integer(3), Reply::OK])
    );
    assert_eq!(client.ask(&["GET", "m"]).await, bulk("x"));
    assert_eq!(client.ask(&["DBSIZE"]).await, Reply::Integer(2));
    stop.cancel();
}
