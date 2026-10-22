//! Pub/sub: a message published reaches every subscriber on the node, in its
//! database and nowhere else, and only a caller the store lets read the space
//! listens (G079 C7, ADR-0133 D1).

use std::time::Duration;

use super::{Client, bulk, error_starting, serving_adjusted, serving_with, status};
use crate::codec::Reply;

/// The reply, or a failed test when none comes within five seconds.
async fn within(client: &mut Client) -> Reply {
    tokio::time::timeout(Duration::from_secs(5), client.reply())
        .await
        .expect("a reply within five seconds")
}

/// A protocol's notice: an array in RESP2, a push in RESP3.
fn notice(three: bool, items: Vec<Reply>) -> Reply {
    if three {
        Reply::Push(items)
    } else {
        Reply::Array(items)
    }
}

fn counted(three: bool, kind: &str, channel: &str, count: i64) -> Reply {
    notice(
        three,
        vec![bulk(kind), bulk(channel), Reply::Integer(count)],
    )
}

#[tokio::test(flavor = "multi_thread")]
async fn a_message_reaches_every_subscriber_and_publish_counts_them() {
    let (address, stop, _) = serving_with("").await;
    let mut exact = Client::to(&address).await;
    let mut patterned = Client::to(&address).await;
    let mut publisher = Client::to(&address).await;
    exact.send(&[b"SUBSCRIBE", b"news", b"sport"]).await;
    assert_eq!(
        within(&mut exact).await,
        counted(false, "subscribe", "news", 1)
    );
    assert_eq!(
        within(&mut exact).await,
        counted(false, "subscribe", "sport", 2)
    );
    assert_eq!(
        patterned.ask(&["PSUBSCRIBE", "new*"]).await,
        counted(false, "psubscribe", "new*", 1)
    );
    assert_eq!(
        publisher.ask(&["PUBLISH", "news", "hello"]).await,
        Reply::Integer(2)
    );
    assert_eq!(
        within(&mut exact).await,
        Reply::Array(vec![bulk("message"), bulk("news"), bulk("hello")])
    );
    assert_eq!(
        within(&mut patterned).await,
        Reply::Array(vec![
            bulk("pmessage"),
            bulk("new*"),
            bulk("news"),
            bulk("hello")
        ])
    );
    // Nobody listens on this one, and nothing is kept for later.
    assert_eq!(
        publisher.ask(&["PUBLISH", "weather", "rain"]).await,
        Reply::Integer(0)
    );
    // A shard channel is a namespace of its own.
    assert_eq!(
        publisher.ask(&["SPUBLISH", "news", "elsewhere"]).await,
        Reply::Integer(0)
    );
    // Leaving one channel answers what is left; leaving them all names each.
    assert_eq!(
        exact.ask(&["UNSUBSCRIBE", "news"]).await,
        counted(false, "unsubscribe", "news", 1)
    );
    assert_eq!(
        publisher.ask(&["PUBLISH", "news", "again"]).await,
        Reply::Integer(1)
    );
    exact.send(&[b"UNSUBSCRIBE"]).await;
    assert_eq!(
        within(&mut exact).await,
        counted(false, "unsubscribe", "sport", 0)
    );
    // Out of subscribed mode, a command is a command again.
    assert_eq!(exact.ask(&["SET", "k", "v"]).await, Reply::OK);
    stop.cancel();
}

#[tokio::test(flavor = "multi_thread")]
async fn subscribed_resp2_takes_only_the_listening_commands() {
    let (address, stop, _) = serving_with("").await;
    let mut client = Client::to(&address).await;
    client.ask(&["SUBSCRIBE", "a"]).await;
    // A notice counts channels and patterns together.
    assert_eq!(
        client.ask(&["PSUBSCRIBE", "b*"]).await,
        counted(false, "psubscribe", "b*", 2)
    );
    let refused = client.ask(&["GET", "k"]).await;
    assert_eq!(
        refused,
        Reply::error(
            "ERR Can't execute 'get': only (P|S)SUBSCRIBE / (P|S)UNSUBSCRIBE / PING / QUIT \
             / RESET are allowed in this context"
        )
    );
    assert_eq!(
        client.ask(&["PING"]).await,
        Reply::Array(vec![bulk("pong"), bulk("")])
    );
    assert_eq!(
        client.ask(&["PING", "hi"]).await,
        Reply::Array(vec![bulk("pong"), bulk("hi")])
    );
    // Nothing subscribed: one notice with no channel.
    let mut idle = Client::to(&address).await;
    assert_eq!(
        idle.ask(&["PUNSUBSCRIBE"]).await,
        Reply::Array(vec![bulk("punsubscribe"), Reply::Nil, Reply::Integer(0)])
    );
    assert!(error_starting(
        &idle.ask(&["SUBSCRIBE"]).await,
        "ERR wrong number of arguments"
    ));
    assert_eq!(client.ask(&["RESET"]).await, status("RESET"));
    assert_eq!(client.ask(&["GET", "k"]).await, Reply::Nil);
    stop.cancel();
}

#[tokio::test(flavor = "multi_thread")]
async fn resp3_pushes_and_keeps_every_command() {
    let (address, stop, _) = serving_with("").await;
    let mut client = Client::to(&address).await;
    client.ask(&["HELLO", "3"]).await;
    assert_eq!(
        client.ask(&["SSUBSCRIBE", "orders"]).await,
        counted(true, "ssubscribe", "orders", 1)
    );
    // Counts are pairs of items in RESP3 too, not a map.
    assert_eq!(
        client.ask(&["PUBSUB", "SHARDNUMSUB", "orders"]).await,
        Reply::Array(vec![bulk("orders"), Reply::Integer(1)])
    );
    assert_eq!(client.ask(&["SET", "k", "v"]).await, Reply::OK);
    let mut publisher = Client::to(&address).await;
    assert_eq!(
        publisher.ask(&["SPUBLISH", "orders", "o-1"]).await,
        Reply::Integer(1)
    );
    assert_eq!(
        within(&mut client).await,
        Reply::Push(vec![bulk("smessage"), bulk("orders"), bulk("o-1")])
    );
    assert_eq!(client.ask(&["GET", "k"]).await, bulk("v"));
    stop.cancel();
}

#[tokio::test(flavor = "multi_thread")]
async fn a_channel_belongs_to_one_database() {
    let (address, stop, db) = serving_with("").await;
    db.session()
        .run("USE NAMESPACE app; DEFINE DATABASE other; USE DATABASE other; DEFINE SPACE cache;")
        .await
        .unwrap();
    let mut here = Client::to(&address).await;
    let mut there = Client::to(&address).await;
    there.ask(&["TESSARIDB.USE", "app", "other", "cache"]).await;
    here.ask(&["SUBSCRIBE", "news"]).await;
    there.ask(&["SUBSCRIBE", "news"]).await;
    let mut publisher = Client::to(&address).await;
    assert_eq!(
        publisher.ask(&["PUBLISH", "news", "main only"]).await,
        Reply::Integer(1)
    );
    assert_eq!(
        within(&mut here).await,
        Reply::Array(vec![bulk("message"), bulk("news"), bulk("main only")])
    );
    // Moving a listening connection to another space would move its channels.
    assert!(error_starting(
        &there.ask(&["TESSARIDB.USE", "app", "main", "cache"]).await,
        "ERR"
    ));
    stop.cancel();
}

#[tokio::test(flavor = "multi_thread")]
async fn pubsub_names_the_channels_and_counts_their_listeners() {
    let (address, stop, _) = serving_with("").await;
    let mut one = Client::to(&address).await;
    let mut two = Client::to(&address).await;
    one.send(&[b"SUBSCRIBE", b"news", b"sport"]).await;
    within(&mut one).await;
    within(&mut one).await;
    two.ask(&["SUBSCRIBE", "news"]).await;
    let mut pattern = Client::to(&address).await;
    pattern.send(&[b"PSUBSCRIBE", b"n*", b"s*"]).await;
    within(&mut pattern).await;
    within(&mut pattern).await;
    let mut shard = Client::to(&address).await;
    shard.ask(&["SSUBSCRIBE", "orders"]).await;
    let mut asking = Client::to(&address).await;
    let mut channels = match asking.ask(&["PUBSUB", "CHANNELS"]).await {
        Reply::Array(items) => items,
        other => vec![other],
    };
    channels.sort_by_key(|reply| format!("{reply:?}"));
    assert_eq!(channels, vec![bulk("news"), bulk("sport")]);
    assert_eq!(
        asking.ask(&["PUBSUB", "CHANNELS", "s*"]).await,
        Reply::Array(vec![bulk("sport")])
    );
    assert_eq!(
        asking.ask(&["PUBSUB", "NUMSUB", "news", "none"]).await,
        Reply::Array(vec![
            bulk("news"),
            Reply::Integer(2),
            bulk("none"),
            Reply::Integer(0)
        ])
    );
    assert_eq!(asking.ask(&["PUBSUB", "NUMPAT"]).await, Reply::Integer(2));
    assert_eq!(
        asking.ask(&["PUBSUB", "SHARDCHANNELS"]).await,
        Reply::Array(vec![bulk("orders")])
    );
    assert_eq!(
        asking.ask(&["PUBSUB", "SHARDNUMSUB", "orders"]).await,
        Reply::Array(vec![bulk("orders"), Reply::Integer(1)])
    );
    // A connection that leaves takes its subscriptions with it.
    drop(two);
    let mut gone = Reply::Nil;
    for _ in 0..50 {
        gone = asking.ask(&["PUBSUB", "NUMSUB", "news"]).await;
        if gone == Reply::Array(vec![bulk("news"), Reply::Integer(1)]) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(gone, Reply::Array(vec![bulk("news"), Reply::Integer(1)]));
    stop.cancel();
}

#[tokio::test(flavor = "multi_thread")]
async fn only_a_reader_listens_and_only_a_writer_publishes() {
    let (address, stop, db) =
        serving_with("DEFINE USER root ROLE owner PASSWORD 'secret-one';").await;
    let mut owner = db.session();
    owner.sign_in("root", "secret-one").await.unwrap();
    owner
        .run("DEFINE USER reader ON app.main ROLE viewer PASSWORD 'secret-two';")
        .await
        .unwrap();
    drop(owner);
    let mut anonymous = Client::to(&address).await;
    assert!(error_starting(
        &anonymous.ask(&["SUBSCRIBE", "news"]).await,
        "NOAUTH"
    ));
    assert!(error_starting(
        &anonymous.ask(&["PUBLISH", "news", "x"]).await,
        "NOAUTH"
    ));
    let mut reader = Client::to(&address).await;
    reader.ask(&["AUTH", "reader", "secret-two"]).await;
    assert!(error_starting(
        &reader.ask(&["PUBLISH", "news", "x"]).await,
        "NOPERM"
    ));
    assert_eq!(
        reader.ask(&["SUBSCRIBE", "news"]).await,
        counted(false, "subscribe", "news", 1)
    );
    let mut root = Client::to(&address).await;
    root.ask(&["AUTH", "root", "secret-one"]).await;
    assert_eq!(root.ask(&["PUBLISH", "news", "x"]).await, Reply::Integer(1));
    assert_eq!(
        within(&mut reader).await,
        Reply::Array(vec![bulk("message"), bulk("news"), bulk("x")])
    );
    stop.cancel();
}

/// Whether the node closed `client`'s connection within five seconds,
/// reading past anything it was sent first.
async fn closed(client: &mut Client) -> bool {
    use tokio::io::AsyncReadExt as _;
    let mut chunk = [0_u8; 4096];
    let deadline = tokio::time::Instant::now()
        .checked_add(Duration::from_secs(5))
        .unwrap();
    loop {
        match tokio::time::timeout_at(deadline, client.stream.read(&mut chunk)).await {
            Ok(Ok(0)) | Ok(Err(_)) => return true,
            Ok(Ok(_)) => continue,
            Err(_) => return false,
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_subscriber_that_does_not_read_is_disconnected_not_skipped() {
    let (address, stop, _) = serving_adjusted("", |node| {
        node.listening_within((4, 1_024 * 1_024), Duration::from_secs(60), 64)
    })
    .await;
    let mut slow = Client::to(&address).await;
    slow.ask(&["SUBSCRIBE", "flood"]).await;
    let mut publisher = Client::to(&address).await;
    // Bigger than what the sockets between them buffer, so the connection's
    // own writes stop and its inbox is what fills.
    let message = "x".repeat(64 * 1024);
    let mut reached = 0;
    for _ in 0..64 {
        if let Reply::Integer(count) = publisher.ask(&["PUBLISH", "flood", &message]).await {
            reached += count;
        }
    }
    assert!(reached < 64, "a full inbox takes nothing more: {reached}");
    assert!(
        closed(&mut slow).await,
        "the subscriber that fell behind is cut off"
    );
    stop.cancel();
}

#[tokio::test(flavor = "multi_thread")]
async fn a_revoked_reader_stops_listening() {
    let (address, stop, db) = serving_adjusted(
        "DEFINE USER root ROLE owner PASSWORD 'secret-one';",
        |node| node.listening_within((64, 1_024 * 1_024), Duration::from_millis(100), 64),
    )
    .await;
    let mut owner = db.session();
    owner.sign_in("root", "secret-one").await.unwrap();
    owner
        .run(
            "DEFINE USER reader ON app.main ROLE viewer PASSWORD 'secret-two'; \
             USE NAMESPACE app; USE DATABASE main; DEFINE SPACE other; \
             GRANT read ON cache TO reader; GRANT read ON other TO reader;",
        )
        .await
        .unwrap();
    let mut reader = Client::to(&address).await;
    reader.ask(&["AUTH", "reader", "secret-two"]).await;
    assert_eq!(
        reader.ask(&["SUBSCRIBE", "news"]).await,
        counted(false, "subscribe", "news", 1)
    );
    owner
        // Revoking the last grant would widen the user back to its role, so
        // one on another table stays.
        .run("REVOKE read ON cache FROM reader;")
        .await
        .unwrap();
    assert!(
        closed(&mut reader).await,
        "a revoked grant ends the subscription"
    );
    stop.cancel();
}

#[tokio::test(flavor = "multi_thread")]
async fn a_connection_listens_on_a_bounded_number_of_names() {
    let (address, stop, _) = serving_adjusted("", |node| {
        node.listening_within((64, 1_024 * 1_024), Duration::from_secs(60), 3)
    })
    .await;
    let mut client = Client::to(&address).await;
    client.send(&[b"SUBSCRIBE", b"a", b"b"]).await;
    within(&mut client).await;
    within(&mut client).await;
    assert_eq!(
        client.ask(&["PSUBSCRIBE", "c*", "d*"]).await,
        Reply::error("ERR a connection listens on at most 3 channels and patterns")
    );
    // Within the bound it still listens, and nothing was half-taken.
    assert_eq!(
        client.ask(&["SSUBSCRIBE", "e"]).await,
        counted(false, "ssubscribe", "e", 1)
    );
    stop.cancel();
}
