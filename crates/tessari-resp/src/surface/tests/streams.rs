//! Streams over the store's own topics: `XADD` appends a message a TessariQL
//! reader sees, an ID is `<appended ms>-<position>`, and ranges, reads, trims
//! and a blocked `XREAD` answer by those IDs (G079 W8e).

use std::time::Duration;

use super::{Client, bulk, error_starting, serving_with};
use crate::codec::Reply;

/// The text of a bulk reply.
fn text(reply: &Reply) -> String {
    assert!(
        matches!(reply, Reply::Bulk(_)),
        "not a bulk reply: {reply:?}"
    );
    match reply {
        Reply::Bulk(bytes) => String::from_utf8(bytes.clone()).unwrap(),
        _ => String::new(),
    }
}

/// An ID's two halves.
fn halves(id: &str) -> (u64, u64) {
    let (ms, position) = id.split_once('-').unwrap();
    (ms.parse().unwrap(), position.parse().unwrap())
}

/// `XREAD`'s answer in RESP2: each stream and its entries, as pairs.
fn streams(read: Vec<(Reply, Reply)>) -> Reply {
    Reply::Array(
        read.into_iter()
            .map(|(key, entries)| Reply::Array(vec![key, entries]))
            .collect(),
    )
}

/// One entry as `XRANGE` answers it.
fn entry(id: &str, fields: &[&str]) -> Reply {
    Reply::Array(vec![
        bulk(id),
        Reply::Array(fields.iter().map(|field| bulk(field)).collect()),
    ])
}

#[tokio::test(flavor = "multi_thread")]
async fn xadd_appends_to_a_topic_and_the_ranges_read_it_back_by_id() {
    let (address, stop, db) = serving_with("").await;
    let mut client = Client::to(&address).await;
    let first = text(
        &client
            .ask(&["XADD", "events", "*", "kind", "paid", "n", "1"])
            .await,
    );
    let second = text(
        &client
            .ask(&["XADD", "events", "*", "kind", "refunded"])
            .await,
    );
    let third = text(&client.ask(&["XADD", "events", "*", "kind", "late"]).await);
    let (ms, position) = halves(&first);
    assert_eq!(position, 1);
    let now = u64::try_from(crate::surface::commands::now_millis()).unwrap();
    assert!(
        ms <= now && now.saturating_sub(ms) < 60_000,
        "{ms} near {now}"
    );
    assert_eq!(halves(&second).1, 2);
    assert_eq!(halves(&third).1, 3);
    assert!(halves(&second).0 >= ms);
    // The message is the topic's own: TessariQL reads it.
    let read = db
        .session()
        .run("USE NAMESPACE app; USE DATABASE main; READ FROM events AFTER 0 LIMIT 1;")
        .await
        .unwrap();
    let shown = format!("{:?}", read.last().unwrap());
    assert!(shown.contains("paid"), "{shown}");
    assert_eq!(client.ask(&["XLEN", "events"]).await, Reply::Integer(3));
    assert_eq!(client.ask(&["XLEN", "missing"]).await, Reply::Integer(0));
    let all = Reply::Array(vec![
        entry(&first, &["kind", "paid", "n", "1"]),
        entry(&second, &["kind", "refunded"]),
        entry(&third, &["kind", "late"]),
    ]);
    assert_eq!(client.ask(&["XRANGE", "events", "-", "+"]).await, all);
    assert_eq!(
        client
            .ask(&["XRANGE", "events", "-", "+", "COUNT", "1"])
            .await,
        Reply::Array(vec![entry(&first, &["kind", "paid", "n", "1"])])
    );
    // By ID, inclusive and exclusive.
    assert_eq!(
        client.ask(&["XRANGE", "events", &second, &third]).await,
        Reply::Array(vec![
            entry(&second, &["kind", "refunded"]),
            entry(&third, &["kind", "late"])
        ])
    );
    let after_first = format!("({first}");
    let before_third = format!("({third}");
    assert_eq!(
        client
            .ask(&["XRANGE", "events", &after_first, &before_third])
            .await,
        Reply::Array(vec![entry(&second, &["kind", "refunded"])])
    );
    // An ID with no sequence is its whole millisecond.
    let (third_ms, _) = halves(&third);
    let later = (third_ms.saturating_add(1)).to_string();
    assert_eq!(
        client.ask(&["XRANGE", "events", &later, "+"]).await,
        Reply::Array(Vec::new())
    );
    assert_eq!(
        client
            .ask(&["XREVRANGE", "events", "+", "-", "COUNT", "2"])
            .await,
        Reply::Array(vec![
            entry(&third, &["kind", "late"]),
            entry(&second, &["kind", "refunded"])
        ])
    );
    assert_eq!(
        client.ask(&["XRANGE", "missing", "-", "+"]).await,
        Reply::Array(Vec::new())
    );
    stop.cancel();
}

#[tokio::test(flavor = "multi_thread")]
async fn a_stream_is_trimmed_and_refuses_what_it_cannot_be() {
    let (address, stop, _) = serving_with("DEFINE TABLE ledger SCHEMALESS;").await;
    let mut client = Client::to(&address).await;
    assert_eq!(
        client
            .ask(&["XADD", "absent", "NOMKSTREAM", "*", "a", "1"])
            .await,
        Reply::Nil
    );
    assert_eq!(client.ask(&["XLEN", "absent"]).await, Reply::Integer(0));
    for n in 1..=4 {
        client
            .ask(&["XADD", "jobs", "*", "n", &n.to_string()])
            .await;
    }
    let kept = text(
        &client
            .ask(&["XADD", "jobs", "MAXLEN", "~", "2", "*", "n", "5"])
            .await,
    );
    assert_eq!(halves(&kept).1, 5);
    assert_eq!(client.ask(&["XLEN", "jobs"]).await, Reply::Integer(2));
    assert_eq!(
        client.ask(&["XTRIM", "jobs", "MAXLEN", "1"]).await,
        Reply::Integer(1)
    );
    assert_eq!(
        client.ask(&["XRANGE", "jobs", "-", "+"]).await,
        Reply::Array(vec![entry(&kept, &["n", "5"])])
    );
    let sixth = text(&client.ask(&["XADD", "jobs", "*", "n", "6"]).await);
    assert_eq!(
        client.ask(&["XTRIM", "jobs", "MINID", &sixth]).await,
        Reply::Integer(1)
    );
    assert_eq!(client.ask(&["XLEN", "jobs"]).await, Reply::Integer(1));
    // A position is the commit's to decide.
    assert!(error_starting(
        &client.ask(&["XADD", "jobs", "1-1", "n", "7"]).await,
        "ERR"
    ));
    // A key is a topic name, and a name is grammar.
    assert!(error_starting(
        &client.ask(&["XADD", "orders:eu", "*", "n", "1"]).await,
        "ERR"
    ));
    assert!(error_starting(
        &client.ask(&["XADD", "jobs", "*", "n"]).await,
        "ERR"
    ));
    assert!(error_starting(
        &client.ask(&["XADD", "ledger", "*", "n", "1"]).await,
        "WRONGTYPE"
    ));
    assert!(error_starting(
        &client.ask(&["XDEL", "jobs", &sixth]).await,
        "ERR"
    ));
    stop.cancel();
}

#[tokio::test(flavor = "multi_thread")]
async fn xread_answers_past_each_id_and_xinfo_describes_the_stream() {
    let (address, stop, _) = serving_with("").await;
    let mut client = Client::to(&address).await;
    let first = text(&client.ask(&["XADD", "a", "*", "x", "1"]).await);
    let mut three = Client::to(&address).await;
    three.ask(&["HELLO", "3"]).await;
    let second = text(&client.ask(&["XADD", "a", "*", "x", "2"]).await);
    let other = text(&client.ask(&["XADD", "b", "*", "y", "1"]).await);
    assert_eq!(
        client
            .ask(&["XREAD", "STREAMS", "a", "b", "0", &other])
            .await,
        streams(vec![(
            bulk("a"),
            Reply::Array(vec![
                entry(&first, &["x", "1"]),
                entry(&second, &["x", "2"])
            ])
        )])
    );
    assert_eq!(
        three
            .ask(&["XREAD", "COUNT", "1", "STREAMS", "a", &first])
            .await,
        Reply::Map(vec![(
            bulk("a"),
            Reply::Array(vec![entry(&second, &["x", "2"])])
        )])
    );
    assert_eq!(
        client.ask(&["XREAD", "STREAMS", "a", "$"]).await,
        Reply::NilArray
    );
    assert_eq!(
        client.ask(&["XREAD", "STREAMS", "a", "+"]).await,
        streams(vec![(
            bulk("a"),
            Reply::Array(vec![entry(&second, &["x", "2"])])
        )])
    );
    let described = three.ask(&["XINFO", "STREAM", "a"]).await;
    assert!(matches!(described, Reply::Map(_)), "{described:?}");
    let info = match described {
        Reply::Map(info) => info,
        _ => Vec::new(),
    };
    let field = |name: &str| {
        info.iter()
            .find(|(key, _)| *key == bulk(name))
            .map(|(_, value)| value.clone())
            .unwrap()
    };
    assert_eq!(field("length"), Reply::Integer(2));
    assert_eq!(field("entries-added"), Reply::Integer(2));
    assert_eq!(field("groups"), Reply::Integer(0));
    assert_eq!(field("last-generated-id"), bulk(&second));
    assert_eq!(field("first-entry"), entry(&first, &["x", "1"]));
    assert_eq!(field("last-entry"), entry(&second, &["x", "2"]));
    assert!(error_starting(
        &client.ask(&["XINFO", "STREAM", "missing"]).await,
        "ERR"
    ));
    stop.cancel();
}

#[tokio::test(flavor = "multi_thread")]
async fn a_blocked_xread_is_woken_by_an_xadd_and_otherwise_times_out() {
    let (address, stop, _) = serving_with("").await;
    let mut reader = Client::to(&address).await;
    let mut writer = Client::to(&address).await;
    writer.ask(&["XADD", "feed", "*", "old", "1"]).await;
    reader
        .send(&[b"XREAD", b"BLOCK", b"0", b"STREAMS", b"feed", b"$"])
        .await;
    assert!(
        tokio::time::timeout(Duration::from_millis(100), reader.reply())
            .await
            .is_err(),
        "answered before anything new was added"
    );
    let added = text(&writer.ask(&["XADD", "feed", "*", "new", "2"]).await);
    let woken = tokio::time::timeout(Duration::from_secs(5), reader.reply())
        .await
        .expect("woken within five seconds");
    assert_eq!(
        woken,
        streams(vec![(
            bulk("feed"),
            Reply::Array(vec![entry(&added, &["new", "2"])])
        )])
    );
    assert_eq!(
        reader
            .ask(&["XREAD", "BLOCK", "50", "STREAMS", "feed", "$"])
            .await,
        Reply::NilArray
    );
    stop.cancel();
}
