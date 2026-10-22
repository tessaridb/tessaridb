//! Hash fields that expire on their own: HEXPIRE and its siblings, answered
//! one code per field (G079 C6, ADR-0132).

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use super::{Client, bulk, error_starting, serving_with, status};
use crate::codec::Reply;

fn codes(items: &[i64]) -> Reply {
    Reply::Array(items.iter().map(|item| Reply::Integer(*item)).collect())
}

/// The integers an array answers, and nothing for any other reply.
fn integers(reply: &Reply) -> Vec<i64> {
    match reply {
        Reply::Array(items) => items
            .iter()
            .filter_map(|item| match item {
                Reply::Integer(value) => Some(*value),
                _ => None,
            })
            .collect(),
        _ => Vec::new(),
    }
}

/// The one integer an array of one answers.
fn only(reply: Reply) -> i64 {
    let found = integers(&reply);
    assert_eq!(found.len(), 1, "{reply:?}");
    found.first().copied().unwrap_or_default()
}

fn now_seconds() -> i64 {
    i64::try_from(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs(),
    )
    .unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn a_field_is_given_a_time_to_live_and_read_back_one_code_per_field() {
    let (address, stop, _) = serving_with("").await;
    let mut client = Client::to(&address).await;
    assert_eq!(
        client
            .ask(&["HEXPIRE", "h", "100", "FIELDS", "2", "a", "b"])
            .await,
        codes(&[-2, -2])
    );
    client
        .ask(&["HSET", "h", "a", "1", "b", "2", "c", "3"])
        .await;
    assert_eq!(
        client
            .ask(&["HEXPIRE", "h", "100", "FIELDS", "2", "a", "zz"])
            .await,
        codes(&[1, -2])
    );
    let left = integers(
        &client
            .ask(&["HTTL", "h", "FIELDS", "3", "a", "b", "zz"])
            .await,
    );
    assert!(matches!(left.as_slice(), [99 | 100, -1, -2]), "{left:?}");
    let millis = only(client.ask(&["HPTTL", "h", "FIELDS", "1", "a"]).await);
    assert!((98_000..=100_000).contains(&millis), "{millis}");
    // The four conditions; a field with no time to live counts as infinite.
    assert_eq!(
        client
            .ask(&["HEXPIRE", "h", "200", "NX", "FIELDS", "2", "a", "b"])
            .await,
        codes(&[0, 1])
    );
    assert_eq!(
        client
            .ask(&["HEXPIRE", "h", "300", "XX", "FIELDS", "2", "b", "c"])
            .await,
        codes(&[1, 0])
    );
    assert_eq!(
        client
            .ask(&["HEXPIRE", "h", "50", "GT", "FIELDS", "2", "a", "c"])
            .await,
        codes(&[0, 0])
    );
    assert_eq!(
        client
            .ask(&["HEXPIRE", "h", "50", "LT", "FIELDS", "2", "a", "c"])
            .await,
        codes(&[1, 1])
    );
    assert_eq!(
        client
            .ask(&["HPERSIST", "h", "FIELDS", "2", "b", "zz"])
            .await,
        codes(&[1, -2])
    );
    assert_eq!(
        client.ask(&["HPERSIST", "h", "FIELDS", "1", "b"]).await,
        codes(&[-1])
    );
    let at = only(client.ask(&["HEXPIRETIME", "h", "FIELDS", "1", "a"]).await);
    assert!((at - now_seconds() - 50).abs() <= 2, "{at}");
    let at = only(client.ask(&["HPEXPIRETIME", "h", "FIELDS", "1", "a"]).await);
    assert!((at / 1_000 - now_seconds() - 50).abs() <= 2, "{at}");
    assert_eq!(
        client.ask(&["HEXPIRETIME", "h", "FIELDS", "1", "b"]).await,
        codes(&[-1])
    );
    // Zero, or an instant already past, removes the field.
    assert_eq!(
        client.ask(&["HEXPIRE", "h", "0", "FIELDS", "1", "c"]).await,
        codes(&[2])
    );
    let past = (now_seconds() - 10).to_string();
    assert_eq!(
        client
            .ask(&["HEXPIREAT", "h", &past, "FIELDS", "1", "b"])
            .await,
        codes(&[2])
    );
    let later = ((now_seconds() + 500) * 1_000).to_string();
    assert_eq!(
        client
            .ask(&["HPEXPIREAT", "h", &later, "FIELDS", "1", "a"])
            .await,
        codes(&[1])
    );
    assert_eq!(
        client.ask(&["HKEYS", "h"]).await,
        Reply::Array(vec![bulk("a")])
    );
    stop.cancel();
}

#[tokio::test(flavor = "multi_thread")]
async fn a_field_past_its_time_is_gone_and_only_an_overwrite_clears_one() {
    let (address, stop, _) = serving_with("").await;
    let mut client = Client::to(&address).await;
    client
        .ask(&["HSET", "h", "a", "1", "n", "5", "s", "x"])
        .await;
    client
        .ask(&["HEXPIRE", "h", "100", "FIELDS", "2", "n", "s"])
        .await;
    client
        .ask(&["HPEXPIRE", "h", "100", "FIELDS", "1", "a"])
        .await;
    // Altered in place, a field keeps its time to live; overwritten, it does not.
    client.ask(&["HINCRBY", "h", "n", "1"]).await;
    client.ask(&["HSET", "h", "s", "y"]).await;
    assert_eq!(
        client.ask(&["HTTL", "h", "FIELDS", "1", "s"]).await,
        codes(&[-1])
    );
    assert!(only(client.ask(&["HTTL", "h", "FIELDS", "1", "n"]).await) > 0);
    tokio::time::sleep(Duration::from_millis(250)).await;
    assert_eq!(client.ask(&["HGET", "h", "a"]).await, Reply::Nil);
    assert_eq!(client.ask(&["HLEN", "h"]).await, Reply::Integer(2));
    // The last field gone, the key is gone.
    client.ask(&["HSET", "o", "x", "1"]).await;
    client
        .ask(&["HPEXPIRE", "o", "100", "FIELDS", "1", "x"])
        .await;
    tokio::time::sleep(Duration::from_millis(250)).await;
    assert_eq!(client.ask(&["EXISTS", "o"]).await, Reply::Integer(0));
    stop.cancel();
}

#[tokio::test(flavor = "multi_thread")]
async fn a_malformed_field_list_or_time_is_refused_as_the_protocol_refuses_it() {
    let (address, stop, _) = serving_with("").await;
    let mut client = Client::to(&address).await;
    client.ask(&["HSET", "h", "a", "1"]).await;
    client.ask(&["RPUSH", "l", "a"]).await;
    let negative = client
        .ask(&["HEXPIRE", "h", "-1", "FIELDS", "1", "a"])
        .await;
    assert!(
        matches!(&negative, Reply::Error(text) if text.contains("must be >= 0")),
        "{negative:?}"
    );
    let mismatch = client
        .ask(&["HEXPIRE", "h", "10", "FIELDS", "2", "a"])
        .await;
    assert!(
        matches!(&mismatch, Reply::Error(text) if text.contains("numfields")),
        "{mismatch:?}"
    );
    let missing = client.ask(&["HEXPIRE", "h", "10", "a"]).await;
    assert!(
        matches!(&missing, Reply::Error(text) if text.contains("FIELDS")),
        "{missing:?}"
    );
    let zero = client.ask(&["HTTL", "h", "FIELDS", "0"]).await;
    assert!(
        matches!(&zero, Reply::Error(text) if text.contains("greater than 0")),
        "{zero:?}"
    );
    assert!(error_starting(
        &client
            .ask(&["HEXPIRE", "l", "10", "FIELDS", "1", "a"])
            .await,
        "WRONGTYPE"
    ));
    assert!(error_starting(
        &client.ask(&["HTTL", "l", "FIELDS", "1", "a"]).await,
        "WRONGTYPE"
    ));
    // Each is a statement, so it queues inside MULTI.
    assert_eq!(client.ask(&["MULTI"]).await, status("OK"));
    assert_eq!(
        client
            .ask(&["HEXPIRE", "h", "10", "FIELDS", "1", "a"])
            .await,
        status("QUEUED")
    );
    assert_eq!(
        client.ask(&["HTTL", "h", "FIELDS", "1", "a"]).await,
        status("QUEUED")
    );
    let exec = client.ask(&["EXEC"]).await;
    assert!(
        matches!(&exec, Reply::Array(items) if items.first() == Some(&codes(&[1]))),
        "{exec:?}"
    );
    stop.cancel();
}
