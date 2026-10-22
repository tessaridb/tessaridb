//! Blocking pops: answered at once when there is something, woken by a commit
//! when there is not, and never handing an element to a client that left
//! (G079 C6).

use std::time::Duration;

use tokio::io::AsyncWriteExt as _;

use super::{Client, bulk, serving_with, status};
use crate::codec::Reply;

/// Long enough for a command to have reached the node and started waiting.
const SETTLE: Duration = Duration::from_millis(100);

/// The reply, or a failed test when none comes within five seconds.
async fn within(client: &mut Client) -> Reply {
    tokio::time::timeout(Duration::from_secs(5), client.reply())
        .await
        .expect("a reply within five seconds")
}

/// Send `words` and read the reply, within five seconds.
async fn ask_within(client: &mut Client, words: &[&str]) -> Reply {
    let words: Vec<&[u8]> = words.iter().map(|word| word.as_bytes()).collect();
    client.send(&words).await;
    within(client).await
}

/// That nothing is answered while `client` waits.
async fn still_waiting(client: &mut Client) {
    assert!(
        tokio::time::timeout(SETTLE, client.reply()).await.is_err(),
        "answered while the key was still empty"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_blocked_pop_is_woken_by_a_push_from_another_client() {
    let (address, stop, _) = serving_with("").await;
    let mut waiter = Client::to(&address).await;
    let mut pusher = Client::to(&address).await;
    waiter.send(&[b"BLPOP", b"empty", b"jobs", b"0"]).await;
    still_waiting(&mut waiter).await;
    pusher.ask(&["RPUSH", "jobs", "a", "b"]).await;
    assert_eq!(
        within(&mut waiter).await,
        Reply::Array(vec![bulk("jobs"), bulk("a")])
    );
    assert_eq!(
        pusher.ask(&["LRANGE", "jobs", "0", "-1"]).await,
        Reply::Array(vec![bulk("b")])
    );
    // Something there already is answered without waiting.
    assert_eq!(
        waiter.ask(&["BRPOP", "jobs", "0"]).await,
        Reply::Array(vec![bulk("jobs"), bulk("b")])
    );
    // A ranking: the member and its score, flat.
    waiter.send(&[b"BZPOPMIN", b"board", b"0"]).await;
    still_waiting(&mut waiter).await;
    pusher.ask(&["ZADD", "board", "2", "x", "1", "y"]).await;
    assert_eq!(
        within(&mut waiter).await,
        Reply::Array(vec![bulk("board"), bulk("y"), bulk("1")])
    );
    // A multi-key pop with a count, and a move.
    waiter
        .send(&[
            b"BLMPOP", b"0", b"2", b"one", b"two", b"LEFT", b"COUNT", b"2",
        ])
        .await;
    still_waiting(&mut waiter).await;
    pusher.ask(&["RPUSH", "two", "p", "q", "r"]).await;
    assert_eq!(
        within(&mut waiter).await,
        Reply::Array(vec![bulk("two"), Reply::Array(vec![bulk("p"), bulk("q")])])
    );
    waiter
        .send(&[b"BLMOVE", b"from", b"to", b"LEFT", b"RIGHT", b"0"])
        .await;
    still_waiting(&mut waiter).await;
    pusher.ask(&["RPUSH", "from", "m"]).await;
    assert_eq!(within(&mut waiter).await, bulk("m"));
    assert_eq!(
        pusher.ask(&["LRANGE", "to", "0", "-1"]).await,
        Reply::Array(vec![bulk("m")])
    );
    stop.cancel();
}

#[tokio::test(flavor = "multi_thread")]
async fn a_blocked_pop_times_out_as_nothing() {
    let (address, stop, _) = serving_with("").await;
    let mut client = Client::to(&address).await;
    assert_eq!(
        ask_within(&mut client, &["BLPOP", "none", "0.05"]).await,
        Reply::NilArray
    );
    assert_eq!(
        ask_within(&mut client, &["BZPOPMAX", "none", "0.05"]).await,
        Reply::NilArray
    );
    assert_eq!(
        ask_within(&mut client, &["BZMPOP", "0.05", "1", "none", "MIN"]).await,
        Reply::NilArray
    );
    assert_eq!(
        ask_within(&mut client, &["BRPOPLPUSH", "none", "to", "0.05"]).await,
        Reply::Nil
    );
    assert!(matches!(
        client.ask(&["BLPOP", "none", "-1"]).await,
        Reply::Error(text) if text.contains("timeout is negative")
    ));
    assert!(matches!(
        client.ask(&["BLPOP", "none", "soon"]).await,
        Reply::Error(text) if text.contains("timeout is not a float")
    ));
    // A command pipelined behind a blocked one is answered after it.
    client
        .stream
        .write_all(b"*3\r\n$5\r\nBLPOP\r\n$4\r\nnone\r\n$4\r\n0.05\r\nPING\r\n")
        .await
        .unwrap();
    assert_eq!(within(&mut client).await, Reply::NilArray);
    assert_eq!(within(&mut client).await, status("PONG"));
    // One pipelined before it is answered at once, not when the wait ends.
    client
        .stream
        .write_all(b"PING\r\n*3\r\n$5\r\nBLPOP\r\n$4\r\nnone\r\n$1\r\n3\r\n")
        .await
        .unwrap();
    let early = tokio::time::timeout(Duration::from_secs(1), client.reply()).await;
    assert_eq!(early.ok(), Some(status("PONG")));
    assert_eq!(within(&mut client).await, Reply::NilArray);
    stop.cancel();
}

#[tokio::test(flavor = "multi_thread")]
async fn a_client_that_left_while_blocked_takes_nothing() {
    let (address, stop, _) = serving_with("").await;
    let mut gone = Client::to(&address).await;
    gone.send(&[b"BLPOP", b"jobs", b"0"]).await;
    still_waiting(&mut gone).await;
    drop(gone);
    let mut pusher = Client::to(&address).await;
    pusher.ask(&["RPUSH", "jobs", "kept"]).await;
    assert_eq!(
        pusher.ask(&["BLPOP", "jobs", "1"]).await,
        Reply::Array(vec![bulk("jobs"), bulk("kept")])
    );
    stop.cancel();
}

#[tokio::test(flavor = "multi_thread")]
async fn inside_multi_a_move_does_not_block_and_a_pop_is_refused() {
    let (address, stop, _) = serving_with("").await;
    let mut client = Client::to(&address).await;
    client.ask(&["MULTI"]).await;
    assert_eq!(
        client
            .ask(&["BLMOVE", "none", "to", "LEFT", "LEFT", "0"])
            .await,
        status("QUEUED")
    );
    assert_eq!(client.ask(&["EXEC"]).await, Reply::Array(vec![Reply::Nil]));
    client.ask(&["MULTI"]).await;
    assert!(matches!(
        client.ask(&["BLPOP", "none", "0"]).await,
        Reply::Error(text) if text.contains("not offered inside MULTI")
    ));
    client.ask(&["DISCARD"]).await;
    stop.cancel();
}

/// Consumers popping while a producer keeps pushing to the same key: every
/// element taken once, and no pop ever told to try again.
#[tokio::test(flavor = "multi_thread")]
async fn consumers_pop_a_list_another_client_keeps_pushing_to() {
    let (address, stop, _) = serving_with("").await;
    let total = 4_000_usize;
    let mut consumers = Vec::new();
    for _ in 0..8 {
        let address = address.clone();
        consumers.push(tokio::spawn(async move {
            let mut client = Client::to(&address).await;
            let mut taken = Vec::new();
            loop {
                let reply = ask_within(&mut client, &["BLMPOP", "1", "1", "work", "LEFT"]).await;
                match reply {
                    Reply::NilArray => return taken,
                    Reply::Array(parts) => match parts.as_slice() {
                        [_, Reply::Array(items)] => taken.extend(items.iter().cloned()),
                        other => return vec![Reply::Error(format!("{other:?}").into())],
                    },
                    other => return vec![other],
                }
            }
        }));
    }
    // Four producers, each sending its pushes in one write, so pushes land
    // while pops are between reading the list and writing it back.
    let mut producers = Vec::new();
    for part in 0..4 {
        let address = address.clone();
        producers.push(tokio::spawn(async move {
            let mut client = Client::to(&address).await;
            let mut burst = Vec::new();
            for item in (part..total).step_by(4) {
                let item = item.to_string();
                burst.extend_from_slice(
                    format!(
                        "*3\r\n$5\r\nRPUSH\r\n$4\r\nwork\r\n${}\r\n{item}\r\n",
                        item.len()
                    )
                    .as_bytes(),
                );
            }
            client.stream.write_all(&burst).await.unwrap();
            for _ in (part..total).step_by(4) {
                let pushed = within(&mut client).await;
                assert!(matches!(pushed, Reply::Integer(_)), "{pushed:?}");
            }
        }));
    }
    for producer in producers {
        producer.await.unwrap();
    }
    let mut every = Vec::new();
    for consumer in consumers {
        every.extend(consumer.await.unwrap());
    }
    let mut expected: Vec<Reply> = (0..total).map(|item| bulk(&item.to_string())).collect();
    let key = |reply: &Reply| format!("{reply:?}");
    every.sort_by_key(key);
    expected.sort_by_key(key);
    assert_eq!(every, expected);
    stop.cancel();
}
