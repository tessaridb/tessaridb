//! What a stream group holds: `XPENDING` reports it, `XCLAIM` and
//! `XAUTOCLAIM` hand it to another member, and `XINFO GROUPS | CONSUMERS`
//! describe the group and its members (G079 W8f).

use super::{Client, bulk, serving_with};
use crate::codec::Reply;

fn entry(id: &str, fields: &[&str]) -> Reply {
    Reply::Array(vec![
        bulk(id),
        Reply::Array(fields.iter().map(|field| bulk(field)).collect()),
    ])
}

async fn add(client: &mut Client, key: &str, value: &str) -> String {
    let added = client.ask(&["XADD", key, "*", "n", value]).await;
    assert!(matches!(added, Reply::Bulk(_)), "{added:?}");
    match added {
        Reply::Bulk(bytes) => String::from_utf8(bytes).unwrap(),
        _ => String::new(),
    }
}

/// `jobs` with three entries, alice holding the first two and bob the third.
async fn held(client: &mut Client) -> [String; 3] {
    let ids = [
        add(client, "jobs", "1").await,
        add(client, "jobs", "2").await,
        add(client, "jobs", "3").await,
    ];
    client.ask(&["XGROUP", "CREATE", "jobs", "g", "0"]).await;
    client
        .ask(&[
            "XREADGROUP",
            "GROUP",
            "g",
            "alice",
            "COUNT",
            "2",
            "STREAMS",
            "jobs",
            ">",
        ])
        .await;
    client
        .ask(&["XREADGROUP", "GROUP", "g", "bob", "STREAMS", "jobs", ">"])
        .await;
    ids
}

/// The rows of an extended `XPENDING`, without their idle times.
fn rows(reply: &Reply) -> Vec<(Reply, Reply, Reply)> {
    let Reply::Array(rows) = reply else {
        return Vec::new();
    };
    rows.iter()
        .filter_map(|row| match row {
            Reply::Array(parts) => match parts.as_slice() {
                [id, member, Reply::Integer(idle), deliveries] if *idle >= 0 => {
                    Some((id.clone(), member.clone(), deliveries.clone()))
                }
                _ => None,
            },
            _ => None,
        })
        .collect()
}

#[tokio::test(flavor = "multi_thread")]
async fn xpending_reports_what_each_member_holds() {
    let (address, stop, _) = serving_with("").await;
    let mut client = Client::to(&address).await;
    let [first, second, third] = held(&mut client).await;
    assert_eq!(
        client.ask(&["XPENDING", "jobs", "g"]).await,
        Reply::Array(vec![
            Reply::Integer(3),
            bulk(&first),
            bulk(&third),
            Reply::Array(vec![
                Reply::Array(vec![bulk("alice"), bulk("2")]),
                Reply::Array(vec![bulk("bob"), bulk("1")]),
            ]),
        ])
    );
    let all = client.ask(&["XPENDING", "jobs", "g", "-", "+", "10"]).await;
    assert_eq!(
        rows(&all),
        vec![
            (bulk(&first), bulk("alice"), Reply::Integer(1)),
            (bulk(&second), bulk("alice"), Reply::Integer(1)),
            (bulk(&third), bulk("bob"), Reply::Integer(1)),
        ]
    );
    let bobs = client
        .ask(&["XPENDING", "jobs", "g", "-", "+", "10", "bob"])
        .await;
    assert_eq!(
        rows(&bobs),
        vec![(bulk(&third), bulk("bob"), Reply::Integer(1))]
    );
    let first_one = client
        .ask(&["XPENDING", "jobs", "g", &second, "+", "1"])
        .await;
    assert_eq!(
        rows(&first_one),
        vec![(bulk(&second), bulk("alice"), Reply::Integer(1))]
    );
    // Held for less than an hour: nothing is that idle.
    assert_eq!(
        client
            .ask(&["XPENDING", "jobs", "g", "IDLE", "3600000", "-", "+", "10"])
            .await,
        Reply::Array(Vec::new())
    );
    client
        .ask(&["XACK", "jobs", "g", &first, &second, &third])
        .await;
    assert_eq!(
        client.ask(&["XPENDING", "jobs", "g"]).await,
        Reply::Array(vec![
            Reply::Integer(0),
            Reply::Nil,
            Reply::Nil,
            Reply::NilArray
        ])
    );
    stop.cancel();
}

#[tokio::test(flavor = "multi_thread")]
async fn xclaim_and_xautoclaim_hand_held_entries_to_another_member() {
    let (address, stop, _) = serving_with("").await;
    let mut client = Client::to(&address).await;
    let [first, second, third] = held(&mut client).await;
    assert_eq!(
        client
            .ask(&["XCLAIM", "jobs", "g", "carol", "0", &first])
            .await,
        Reply::Array(vec![entry(&first, &["n", "1"])])
    );
    assert_eq!(
        client
            .ask(&["XCLAIM", "jobs", "g", "carol", "0", &second, "JUSTID"])
            .await,
        Reply::Array(vec![bulk(&second)])
    );
    // An hour's idleness is asked for and nobody has been idle that long.
    assert_eq!(
        client
            .ask(&["XCLAIM", "jobs", "g", "carol", "3600000", &third])
            .await,
        Reply::Array(Vec::new())
    );
    let carols = client
        .ask(&["XPENDING", "jobs", "g", "-", "+", "10", "carol"])
        .await;
    assert_eq!(rows(&carols).len(), 2);
    assert_eq!(
        client
            .ask(&["XAUTOCLAIM", "jobs", "g", "dave", "0", "0-0", "COUNT", "10"])
            .await,
        Reply::Array(vec![
            bulk("0-0"),
            Reply::Array(vec![
                entry(&first, &["n", "1"]),
                entry(&second, &["n", "2"]),
                entry(&third, &["n", "3"]),
            ]),
            Reply::Array(Vec::new()),
        ])
    );
    let daves = client
        .ask(&["XPENDING", "jobs", "g", "-", "+", "10", "dave"])
        .await;
    assert_eq!(rows(&daves).len(), 3);
    stop.cancel();
}

#[tokio::test(flavor = "multi_thread")]
async fn xinfo_describes_a_streams_groups_and_a_groups_members() {
    let (address, stop, _) = serving_with("").await;
    let mut client = Client::to(&address).await;
    let [_, _, third] = held(&mut client).await;
    client.ask(&["HELLO", "3"]).await;
    let field = |map: &Reply, name: &str| match map {
        Reply::Map(pairs) => pairs
            .iter()
            .find(|(key, _)| *key == bulk(name))
            .map(|(_, value)| value.clone()),
        _ => None,
    };
    let groups = client.ask(&["XINFO", "GROUPS", "jobs"]).await;
    assert!(matches!(groups, Reply::Array(_)), "{groups:?}");
    let groups = match groups {
        Reply::Array(groups) => groups,
        _ => Vec::new(),
    };
    assert_eq!(groups.len(), 1);
    let group = &groups[0];
    assert_eq!(field(group, "name"), Some(bulk("g")));
    assert_eq!(field(group, "consumers"), Some(Reply::Integer(2)));
    assert_eq!(field(group, "pending"), Some(Reply::Integer(3)));
    assert_eq!(field(group, "last-delivered-id"), Some(bulk(&third)));
    assert_eq!(field(group, "entries-read"), Some(Reply::Integer(3)));
    assert_eq!(field(group, "lag"), Some(Reply::Integer(0)));
    let members = client.ask(&["XINFO", "CONSUMERS", "jobs", "g"]).await;
    assert!(matches!(members, Reply::Array(_)), "{members:?}");
    let members = match members {
        Reply::Array(members) => members,
        _ => Vec::new(),
    };
    let named: Vec<_> = members
        .iter()
        .map(|member| (field(member, "name"), field(member, "pending")))
        .collect();
    assert_eq!(
        named,
        vec![
            (Some(bulk("alice")), Some(Reply::Integer(2))),
            (Some(bulk("bob")), Some(Reply::Integer(1))),
        ]
    );
    assert!(
        members
            .iter()
            .all(|member| matches!(field(member, "idle"), Some(Reply::Integer(idle)) if idle >= 0))
    );
    stop.cancel();
}
