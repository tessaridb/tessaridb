//! Stream groups over the store's topic groups: `XGROUP` declares one,
//! `XREADGROUP` hands entries to a member and keeps them held until `XACK`,
//! and a member reads back what it holds (G079 W8f).

use std::time::Duration;

use super::{Client, bulk, error_starting, serving_with};
use crate::codec::Reply;

/// One entry as a stream command answers it.
fn entry(id: &str, fields: &[&str]) -> Reply {
    Reply::Array(vec![
        bulk(id),
        Reply::Array(fields.iter().map(|field| bulk(field)).collect()),
    ])
}

/// A read's answer in RESP2: each stream and its entries.
fn streams(read: Vec<(&str, Vec<Reply>)>) -> Reply {
    Reply::Array(
        read.into_iter()
            .map(|(key, entries)| Reply::Array(vec![bulk(key), Reply::Array(entries)]))
            .collect(),
    )
}

/// The ID `XADD` answered.
async fn add(client: &mut Client, key: &str, field: &str, value: &str) -> String {
    let added = client.ask(&["XADD", key, "*", field, value]).await;
    assert!(matches!(added, Reply::Bulk(_)), "{added:?}");
    match added {
        Reply::Bulk(bytes) => String::from_utf8(bytes).unwrap(),
        _ => String::new(),
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_group_hands_each_entry_to_one_member_and_holds_it_until_acked() {
    let (address, stop, _) = serving_with("").await;
    let mut client = Client::to(&address).await;
    let first = add(&mut client, "jobs", "n", "1").await;
    let second = add(&mut client, "jobs", "n", "2").await;
    let third = add(&mut client, "jobs", "n", "3").await;
    assert_eq!(
        client
            .ask(&["XGROUP", "CREATE", "jobs", "workers", "0"])
            .await,
        Reply::OK
    );
    assert!(error_starting(
        &client
            .ask(&["XGROUP", "CREATE", "jobs", "workers", "0"])
            .await,
        "BUSYGROUP"
    ));
    assert_eq!(
        client
            .ask(&[
                "XREADGROUP",
                "GROUP",
                "workers",
                "alice",
                "COUNT",
                "2",
                "STREAMS",
                "jobs",
                ">",
            ])
            .await,
        streams(vec![(
            "jobs",
            vec![entry(&first, &["n", "1"]), entry(&second, &["n", "2"])]
        )])
    );
    assert_eq!(
        client
            .ask(&[
                "XREADGROUP",
                "GROUP",
                "workers",
                "bob",
                "STREAMS",
                "jobs",
                ">"
            ])
            .await,
        streams(vec![("jobs", vec![entry(&third, &["n", "3"])])])
    );
    // Nothing new is nothing at all.
    assert_eq!(
        client
            .ask(&[
                "XREADGROUP",
                "GROUP",
                "workers",
                "bob",
                "STREAMS",
                "jobs",
                ">"
            ])
            .await,
        Reply::NilArray
    );
    // A member reads back what it holds, and only that.
    assert_eq!(
        client
            .ask(&[
                "XREADGROUP",
                "GROUP",
                "workers",
                "alice",
                "STREAMS",
                "jobs",
                "0"
            ])
            .await,
        streams(vec![(
            "jobs",
            vec![entry(&first, &["n", "1"]), entry(&second, &["n", "2"])]
        )])
    );
    assert_eq!(
        client
            .ask(&["XACK", "jobs", "workers", &first, &first])
            .await,
        Reply::Integer(1)
    );
    assert_eq!(
        client
            .ask(&[
                "XREADGROUP",
                "GROUP",
                "workers",
                "alice",
                "STREAMS",
                "jobs",
                "0"
            ])
            .await,
        streams(vec![("jobs", vec![entry(&second, &["n", "2"])])])
    );
    // Past what it holds, a member holds nothing — and is still answered.
    assert_eq!(
        client
            .ask(&[
                "XREADGROUP",
                "GROUP",
                "workers",
                "alice",
                "STREAMS",
                "jobs",
                &second,
            ])
            .await,
        streams(vec![("jobs", Vec::new())])
    );
    assert!(error_starting(
        &client
            .ask(&[
                "XREADGROUP",
                "GROUP",
                "nobody",
                "alice",
                "STREAMS",
                "jobs",
                ">"
            ])
            .await,
        "NOGROUP"
    ));
    assert_eq!(
        client.ask(&["XACK", "jobs", "nobody", &first]).await,
        Reply::Integer(0)
    );
    stop.cancel();
}

#[tokio::test(flavor = "multi_thread")]
async fn xgroup_declares_starts_and_drops_a_group_and_its_members() {
    let (address, stop, _) = serving_with("").await;
    let mut client = Client::to(&address).await;
    assert!(error_starting(
        &client.ask(&["XGROUP", "CREATE", "absent", "g", "$"]).await,
        "ERR"
    ));
    assert_eq!(
        client
            .ask(&["XGROUP", "CREATE", "fresh", "g", "$", "MKSTREAM"])
            .await,
        Reply::OK
    );
    assert_eq!(client.ask(&["XLEN", "fresh"]).await, Reply::Integer(0));
    let old = add(&mut client, "feed", "n", "1").await;
    // `$` starts past what is there.
    assert_eq!(
        client.ask(&["XGROUP", "CREATE", "feed", "late", "$"]).await,
        Reply::OK
    );
    let new = add(&mut client, "feed", "n", "2").await;
    assert_eq!(
        client
            .ask(&["XREADGROUP", "GROUP", "late", "c", "STREAMS", "feed", ">"])
            .await,
        streams(vec![("feed", vec![entry(&new, &["n", "2"])])])
    );
    // SETID moves where the group hands out from.
    assert_eq!(
        client.ask(&["XGROUP", "SETID", "feed", "late", "0"]).await,
        Reply::OK
    );
    assert_eq!(
        client
            .ask(&[
                "XREADGROUP",
                "GROUP",
                "late",
                "c",
                "COUNT",
                "1",
                "STREAMS",
                "feed",
                ">",
            ])
            .await,
        streams(vec![("feed", vec![entry(&old, &["n", "1"])])])
    );
    assert_eq!(
        client
            .ask(&["XGROUP", "CREATECONSUMER", "feed", "late", "d"])
            .await,
        Reply::Integer(1)
    );
    assert_eq!(
        client
            .ask(&["XGROUP", "CREATECONSUMER", "feed", "late", "d"])
            .await,
        Reply::Integer(0)
    );
    assert_eq!(
        client
            .ask(&["XGROUP", "DELCONSUMER", "feed", "late", "c"])
            .await,
        Reply::Integer(1)
    );
    // NOACK hands out and settles at once: nothing is held. The group was
    // set back, so the entry after the one replayed comes first.
    let noack = add(&mut client, "feed", "n", "3").await;
    assert_eq!(
        client
            .ask(&[
                "XREADGROUP",
                "GROUP",
                "late",
                "d",
                "NOACK",
                "STREAMS",
                "feed",
                ">",
            ])
            .await,
        streams(vec![(
            "feed",
            vec![entry(&new, &["n", "2"]), entry(&noack, &["n", "3"])]
        )])
    );
    assert_eq!(
        client
            .ask(&["XREADGROUP", "GROUP", "late", "d", "STREAMS", "feed", "0"])
            .await,
        streams(vec![("feed", Vec::new())])
    );
    assert_eq!(
        client.ask(&["XGROUP", "DESTROY", "feed", "late"]).await,
        Reply::Integer(1)
    );
    assert_eq!(
        client.ask(&["XGROUP", "DESTROY", "feed", "late"]).await,
        Reply::Integer(0)
    );
    stop.cancel();
}

#[tokio::test(flavor = "multi_thread")]
async fn a_blocked_xreadgroup_is_woken_by_an_xadd() {
    let (address, stop, _) = serving_with("").await;
    let mut reader = Client::to(&address).await;
    let mut writer = Client::to(&address).await;
    writer
        .ask(&["XGROUP", "CREATE", "inbox", "g", "$", "MKSTREAM"])
        .await;
    reader
        .send(&[
            b"XREADGROUP",
            b"GROUP",
            b"g",
            b"c",
            b"BLOCK",
            b"0",
            b"STREAMS",
            b"inbox",
            b">",
        ])
        .await;
    assert!(
        tokio::time::timeout(Duration::from_millis(100), reader.reply())
            .await
            .is_err(),
        "answered before anything was added"
    );
    let added = add(&mut writer, "inbox", "m", "1").await;
    let woken = tokio::time::timeout(Duration::from_secs(5), reader.reply())
        .await
        .expect("woken within five seconds");
    assert_eq!(
        woken,
        streams(vec![("inbox", vec![entry(&added, &["m", "1"])])])
    );
    stop.cancel();
}
