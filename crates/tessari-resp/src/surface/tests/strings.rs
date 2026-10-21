//! The string commands against the store's own space.

use super::{Client, bulk, error_starting, serving_with, status};
use crate::codec::Reply;

#[tokio::test(flavor = "multi_thread")]
async fn a_value_reads_back_as_the_bytes_it_was_written_as() {
    let (address, stop, db) = serving_with("SET cache:'native' = 7;").await;
    let mut client = Client::to(&address).await;
    client.send(&[b"SET", b"bin", b"a\0\xff\r\nz"]).await;
    assert_eq!(client.reply().await, Reply::OK);
    client.send(&[b"GET", b"bin"]).await;
    assert_eq!(client.reply().await, Reply::Bulk(b"a\0\xff\r\nz".to_vec()));
    for value in ["42", "-7", "007", "1.5", "hello"] {
        assert_eq!(client.ask(&["SET", "k", value]).await, Reply::OK);
        assert_eq!(client.ask(&["GET", "k"]).await, bulk(value), "{value}");
    }
    assert_eq!(client.ask(&["GET", "missing"]).await, Reply::Nil);
    // Written from TessariQL, read here; written here, read there.
    assert_eq!(client.ask(&["GET", "native"]).await, bulk("7"));
    assert_eq!(client.ask(&["SET", "count", "10"]).await, Reply::OK);
    let mut session = db.session();
    let read = session
        .run("USE NAMESPACE app; USE DATABASE main; INCR cache:'count' BY 5;")
        .await
        .unwrap();
    assert_eq!(
        format!("{:?}", read.last().unwrap()),
        "Value(Number(Integer(15)))"
    );
    stop.cancel();
}

#[tokio::test(flavor = "multi_thread")]
async fn incr_is_the_stores_own_and_refuses_what_is_not_an_integer() {
    let (address, stop, _) = serving_with("").await;
    let mut client = Client::to(&address).await;
    assert_eq!(client.ask(&["INCR", "n"]).await, Reply::Integer(1));
    assert_eq!(client.ask(&["INCRBY", "n", "41"]).await, Reply::Integer(42));
    assert_eq!(client.ask(&["DECRBY", "n", "2"]).await, Reply::Integer(40));
    assert_eq!(client.ask(&["DECR", "n"]).await, Reply::Integer(39));
    client.ask(&["SET", "word", "abc"]).await;
    let refused = client.ask(&["INCR", "word"]).await;
    assert_eq!(
        refused,
        Reply::error("ERR value is not an integer or out of range")
    );
    client.ask(&["SET", "top", "9223372036854775807"]).await;
    assert!(error_starting(&client.ask(&["INCR", "top"]).await, "ERR"));
    assert!(error_starting(
        &client.ask(&["INCRBY", "n", "x"]).await,
        "ERR value is not an integer"
    ));
    assert_eq!(
        client.ask(&["INCRBYFLOAT", "f", "10.5"]).await,
        bulk("10.5")
    );
    assert_eq!(client.ask(&["INCRBYFLOAT", "f", "0.1"]).await, bulk("10.6"));
    assert!(error_starting(
        &client.ask(&["INCRBYFLOAT", "word", "1"]).await,
        "ERR value is not a valid float"
    ));
    stop.cancel();
}

#[tokio::test(flavor = "multi_thread")]
async fn set_takes_its_conditions_its_expiries_and_get() {
    let (address, stop, _) = serving_with("").await;
    let mut client = Client::to(&address).await;
    assert_eq!(client.ask(&["SET", "k", "1", "NX"]).await, Reply::OK);
    assert_eq!(client.ask(&["SET", "k", "2", "NX"]).await, Reply::Nil);
    assert_eq!(client.ask(&["SET", "absent", "2", "XX"]).await, Reply::Nil);
    assert_eq!(client.ask(&["SET", "k", "3", "XX", "GET"]).await, bulk("1"));
    assert_eq!(client.ask(&["SETNX", "k", "4"]).await, Reply::Integer(0));
    assert_eq!(
        client.ask(&["SETNX", "fresh", "4"]).await,
        Reply::Integer(1)
    );
    assert_eq!(client.ask(&["SET", "t", "v", "EX", "100"]).await, Reply::OK);
    assert!(matches!(
        client.ask(&["TTL", "t"]).await,
        Reply::Integer(99 | 100)
    ));
    // KEEPTTL keeps it; a plain SET clears it.
    assert_eq!(client.ask(&["SET", "t", "w", "KEEPTTL"]).await, Reply::OK);
    assert!(matches!(
        client.ask(&["TTL", "t"]).await,
        Reply::Integer(99 | 100)
    ));
    assert_eq!(client.ask(&["GET", "t"]).await, bulk("w"));
    assert_eq!(
        client.ask(&["SET", "nottl", "v", "KEEPTTL"]).await,
        Reply::OK
    );
    assert_eq!(client.ask(&["TTL", "nottl"]).await, Reply::Integer(-1));
    assert_eq!(client.ask(&["SET", "t", "x"]).await, Reply::OK);
    assert_eq!(client.ask(&["TTL", "t"]).await, Reply::Integer(-1));
    assert_eq!(client.ask(&["SETEX", "s", "50", "v"]).await, Reply::OK);
    assert!(matches!(
        client.ask(&["PTTL", "s"]).await,
        Reply::Integer(49_000..=50_000)
    ));
    assert!(error_starting(
        &client.ask(&["SET", "k", "v", "EX", "0"]).await,
        "ERR invalid expire time"
    ));
    assert!(error_starting(
        &client.ask(&["SET", "k", "v", "NX", "XX"]).await,
        "ERR syntax error"
    ));
    assert_eq!(client.ask(&["GETSET", "k", "new"]).await, bulk("3"));
    assert_eq!(client.ask(&["GETDEL", "k"]).await, bulk("new"));
    assert_eq!(client.ask(&["GET", "k"]).await, Reply::Nil);
    stop.cancel();
}

#[tokio::test(flavor = "multi_thread")]
async fn the_commands_that_change_bytes_keep_the_expiry_and_lose_no_write() {
    let (address, stop, _) = serving_with("").await;
    let mut client = Client::to(&address).await;
    assert_eq!(
        client.ask(&["APPEND", "a", "Hello"]).await,
        Reply::Integer(5)
    );
    client.ask(&["EXPIRE", "a", "100"]).await;
    assert_eq!(
        client.ask(&["APPEND", "a", " World"]).await,
        Reply::Integer(11)
    );
    assert_eq!(client.ask(&["GET", "a"]).await, bulk("Hello World"));
    assert!(matches!(
        client.ask(&["TTL", "a"]).await,
        Reply::Integer(99 | 100)
    ));
    assert_eq!(
        client.ask(&["SETRANGE", "a", "6", "There"]).await,
        Reply::Integer(11)
    );
    assert_eq!(
        client.ask(&["GETRANGE", "a", "0", "4"]).await,
        bulk("Hello")
    );
    assert_eq!(
        client.ask(&["GETRANGE", "a", "-5", "-1"]).await,
        bulk("There")
    );
    assert_eq!(client.ask(&["STRLEN", "a"]).await, Reply::Integer(11));
    assert_eq!(client.ask(&["STRLEN", "none"]).await, Reply::Integer(0));
    // Concurrent appenders: every append lands, none overwrites another.
    let mut writers = Vec::new();
    for writer in 0..4 {
        let address = address.clone();
        writers.push(tokio::spawn(async move {
            let mut client = Client::to(&address).await;
            for _ in 0..25 {
                let reply = client.ask(&["APPEND", "log", "x"]).await;
                assert!(
                    matches!(reply, Reply::Integer(_)),
                    "writer {writer}: {reply:?}"
                );
            }
        }));
    }
    for writer in writers {
        writer.await.unwrap();
    }
    assert_eq!(client.ask(&["STRLEN", "log"]).await, Reply::Integer(100));
    stop.cancel();
}

#[tokio::test(flavor = "multi_thread")]
async fn several_keys_at_once() {
    let (address, stop, _) = serving_with("").await;
    let mut client = Client::to(&address).await;
    assert_eq!(client.ask(&["MSET", "a", "1", "b", "2"]).await, Reply::OK);
    assert_eq!(
        client.ask(&["MGET", "a", "missing", "b"]).await,
        Reply::Array(vec![bulk("1"), Reply::Nil, bulk("2")])
    );
    // All or nothing: `b` is there, so `c` is not written.
    assert_eq!(
        client.ask(&["MSETNX", "c", "3", "b", "9"]).await,
        Reply::Integer(0)
    );
    assert_eq!(client.ask(&["GET", "c"]).await, Reply::Nil);
    assert_eq!(client.ask(&["GET", "b"]).await, bulk("2"));
    assert_eq!(
        client.ask(&["MSETNX", "c", "3", "d", "4"]).await,
        Reply::Integer(1)
    );
    assert_eq!(client.ask(&["GET", "d"]).await, bulk("4"));
    assert_eq!(client.ask(&["SET", "x", "ohmytext"]).await, Reply::OK);
    assert_eq!(client.ask(&["SET", "y", "mynewtext"]).await, Reply::OK);
    assert_eq!(client.ask(&["LCS", "x", "y"]).await, bulk("mytext"));
    assert_eq!(
        client.ask(&["LCS", "x", "y", "LEN"]).await,
        Reply::Integer(6)
    );
    assert_eq!(
        client.ask(&["GETEX", "x", "EX", "100"]).await,
        bulk("ohmytext")
    );
    assert!(matches!(
        client.ask(&["TTL", "x"]).await,
        Reply::Integer(99 | 100)
    ));
    assert_eq!(
        client.ask(&["GETEX", "x", "PERSIST"]).await,
        bulk("ohmytext")
    );
    assert_eq!(client.ask(&["TTL", "x"]).await, Reply::Integer(-1));
    assert_eq!(
        client.ask(&["MSET", "odd"]).await,
        Reply::error("ERR wrong number of arguments for 'mset' command")
    );
    assert_eq!(client.ask(&["PING"]).await, status("PONG"));
    stop.cancel();
}
