//! Keys: existence, expiry, moves and walks.

use std::collections::BTreeSet;

use super::{Client, bulk, error_starting, serving_with, status};
use crate::codec::Reply;

#[tokio::test(flavor = "multi_thread")]
async fn existence_expiry_and_removal() {
    let (address, stop, _) = serving_with("").await;
    let mut client = Client::to(&address).await;
    client.ask(&["MSET", "a", "1", "b", "2"]).await;
    assert_eq!(
        client.ask(&["EXISTS", "a", "b", "a", "z"]).await,
        Reply::Integer(3)
    );
    assert_eq!(client.ask(&["TTL", "z"]).await, Reply::Integer(-2));
    assert_eq!(client.ask(&["TTL", "a"]).await, Reply::Integer(-1));
    assert_eq!(client.ask(&["EXPIRE", "a", "100"]).await, Reply::Integer(1));
    assert_eq!(client.ask(&["EXPIRE", "z", "100"]).await, Reply::Integer(0));
    assert!(matches!(
        client.ask(&["TTL", "a"]).await,
        Reply::Integer(99 | 100)
    ));
    // NX on a key that has an expiry does nothing; GT a sooner one does nothing.
    assert_eq!(
        client.ask(&["EXPIRE", "a", "500", "NX"]).await,
        Reply::Integer(0)
    );
    assert_eq!(
        client.ask(&["EXPIRE", "a", "50", "GT"]).await,
        Reply::Integer(0)
    );
    assert_eq!(
        client.ask(&["EXPIRE", "a", "500", "GT"]).await,
        Reply::Integer(1)
    );
    assert!(matches!(
        client.ask(&["TTL", "a"]).await,
        Reply::Integer(499 | 500)
    ));
    let soon = (super::super::commands::now_millis() / 1000 + 300).to_string();
    assert_eq!(
        client.ask(&["EXPIREAT", "b", &soon]).await,
        Reply::Integer(1)
    );
    assert!(matches!(
        client.ask(&["TTL", "b"]).await,
        Reply::Integer(299 | 300)
    ));
    assert!(matches!(
        client.ask(&["EXPIRETIME", "b"]).await,
        Reply::Integer(at) if at.to_string() == soon || (at + 1).to_string() == soon
    ));
    assert_eq!(client.ask(&["PERSIST", "b"]).await, Reply::Integer(1));
    assert_eq!(client.ask(&["PERSIST", "b"]).await, Reply::Integer(0));
    // A past expiry removes the key, as the protocol's own does.
    assert_eq!(client.ask(&["EXPIRE", "b", "-1"]).await, Reply::Integer(1));
    assert_eq!(client.ask(&["EXISTS", "b"]).await, Reply::Integer(0));
    assert_eq!(client.ask(&["DEL", "a", "b", "a"]).await, Reply::Integer(1));
    assert_eq!(client.ask(&["TYPE", "a"]).await, status("none"));
    client.ask(&["SET", "n", "12"]).await;
    assert_eq!(client.ask(&["TYPE", "n"]).await, status("string"));
    assert_eq!(client.ask(&["OBJECT", "ENCODING", "n"]).await, bulk("int"));
    assert_eq!(client.ask(&["DBSIZE"]).await, Reply::Integer(1));
    stop.cancel();
}

#[tokio::test(flavor = "multi_thread")]
async fn a_key_moves_with_its_expiry_and_a_move_onto_a_key_is_refused_by_nx() {
    let (address, stop, _) = serving_with("").await;
    let mut client = Client::to(&address).await;
    client.ask(&["SET", "from", "v", "EX", "100"]).await;
    assert_eq!(client.ask(&["RENAME", "from", "to"]).await, Reply::OK);
    assert_eq!(client.ask(&["GET", "to"]).await, bulk("v"));
    assert_eq!(client.ask(&["EXISTS", "from"]).await, Reply::Integer(0));
    assert!(matches!(
        client.ask(&["TTL", "to"]).await,
        Reply::Integer(99 | 100)
    ));
    assert_eq!(
        client.ask(&["RENAME", "missing", "x"]).await,
        Reply::error("ERR no such key")
    );
    client.ask(&["SET", "taken", "t"]).await;
    assert_eq!(
        client.ask(&["RENAMENX", "to", "taken"]).await,
        Reply::Integer(0)
    );
    assert_eq!(client.ask(&["GET", "to"]).await, bulk("v"));
    assert_eq!(
        client.ask(&["RENAMENX", "to", "free"]).await,
        Reply::Integer(1)
    );
    assert_eq!(client.ask(&["GET", "free"]).await, bulk("v"));
    // A key with no expiry moves as one with none.
    client.ask(&["SET", "plain", "p"]).await;
    assert_eq!(client.ask(&["RENAME", "plain", "moved"]).await, Reply::OK);
    assert_eq!(client.ask(&["TTL", "moved"]).await, Reply::Integer(-1));
    assert_eq!(
        client.ask(&["COPY", "moved", "taken"]).await,
        Reply::Integer(0)
    );
    assert_eq!(
        client.ask(&["COPY", "moved", "taken", "REPLACE"]).await,
        Reply::Integer(1)
    );
    assert_eq!(client.ask(&["GET", "taken"]).await, bulk("p"));
    assert_eq!(
        client.ask(&["COPY", "missing", "y"]).await,
        Reply::Integer(0)
    );
    assert!(error_starting(
        &client.ask(&["COPY", "moved", "moved"]).await,
        "ERR source and destination"
    ));
    stop.cancel();
}

#[tokio::test(flavor = "multi_thread")]
async fn keys_and_scan_walk_every_key_once_text_and_bytes_alike() {
    let (address, stop, _) = serving_with("").await;
    let mut client = Client::to(&address).await;
    let mut written = BTreeSet::new();
    for index in 0..250 {
        let key = format!("user:{index}");
        client.ask(&["SET", &key, "v"]).await;
        written.insert(key.into_bytes());
    }
    client.ask(&["SET", "other", "v"]).await;
    client.send(&[b"SET", b"user:\xff", b"v"]).await;
    assert_eq!(client.reply().await, Reply::OK);
    written.insert(b"user:\xff".to_vec());
    let listed: BTreeSet<Vec<u8>> = array(client.ask(&["KEYS", "user:*"]).await)
        .into_iter()
        .map(bytes_of)
        .collect();
    assert_eq!(
        listed, written,
        "KEYS lists every matching key and no other"
    );
    assert_eq!(
        client.ask(&["KEYS", "user:1?"]).await,
        Reply::Array(
            (10..20)
                .map(|index| bulk(&format!("user:{index}")))
                .collect()
        )
    );
    let mut cursor = "0".to_owned();
    let mut scanned = Vec::new();
    let mut rounds = 0;
    loop {
        let page = array(
            client
                .ask(&["SCAN", &cursor, "MATCH", "user:*", "COUNT", "40"])
                .await,
        );
        assert_eq!(page.len(), 2, "a cursor and a page");
        let mut page = page.into_iter();
        cursor = String::from_utf8(bytes_of(page.next().unwrap())).unwrap();
        if let Some(Reply::Array(keys)) = page.next() {
            scanned.extend(keys.into_iter().map(bytes_of));
        }
        rounds += 1;
        if cursor == "0" {
            break;
        }
    }
    assert!(rounds > 1, "a page of 40 walks 252 keys in several rounds");
    let unique: BTreeSet<Vec<u8>> = scanned.iter().cloned().collect();
    assert_eq!(unique.len(), scanned.len(), "no key is scanned twice");
    assert_eq!(unique, written);
    assert_eq!(
        client.ask(&["SCAN", "123456", "COUNT", "5"]).await,
        Reply::error("ERR invalid cursor")
    );
    assert!(matches!(client.ask(&["RANDOMKEY"]).await, Reply::Bulk(_)));
    stop.cancel();
}

fn bytes_of(reply: Reply) -> Vec<u8> {
    match reply {
        Reply::Bulk(bytes) => bytes,
        other => format!("{other:?}").into_bytes(),
    }
}

/// The items of an array reply; none for any other, which the assertions
/// on them then catch.
fn array(reply: Reply) -> Vec<Reply> {
    match reply {
        Reply::Array(items) => items,
        _ => Vec::new(),
    }
}
