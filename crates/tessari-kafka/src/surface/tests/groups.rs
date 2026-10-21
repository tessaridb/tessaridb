//! Groups and their offsets, and topics made and dropped, as a client asks.

use tokio::net::TcpStream;

use super::{ask, fetch_body, fetched, produce_body, produced, serving, serving_with};
use crate::codec::{Reader, Writer};
use crate::records::{Outgoing, encode_batch};

/// A join's answer: code, generation, leader, own id, and the members the leader is told of.
pub(super) struct Joined {
    pub(super) code: i16,
    pub(super) generation: i32,
    leader: String,
    member: String,
    members: Vec<String>,
}

async fn join(stream: &mut TcpStream, version: i16, group: &str, member: &str) -> Joined {
    join_with(stream, version, group, member, b"subscription").await
}

pub(super) async fn join_with(
    stream: &mut TcpStream,
    version: i16,
    group: &str,
    member: &str,
    subscription: &[u8],
) -> Joined {
    let flexible = version >= 6;
    let mut body = Writer::new();
    body.string(flexible, group).unwrap();
    body.i32(10_000);
    body.i32(10_000);
    body.string(flexible, member).unwrap();
    if version >= 5 {
        body.nullable_string(flexible, None).unwrap();
    }
    body.string(flexible, "consumer").unwrap();
    body.array_len(flexible, Some(1)).unwrap();
    body.string(flexible, "range").unwrap();
    body.bytes(flexible, subscription).unwrap();
    if flexible {
        body.no_tagged_fields();
        body.no_tagged_fields();
    }
    let answer = ask(stream, 11, version, flexible, &body.into_bytes()).await;
    let mut reader = Reader::new(&answer);
    reader.i32("throttle").unwrap();
    let code = reader.i16("error").unwrap();
    let generation = reader.i32("generation").unwrap();
    if version >= 7 {
        reader.nullable_string(flexible, "type").unwrap();
        reader.nullable_string(flexible, "protocol").unwrap();
    } else {
        reader.string(flexible, "protocol").unwrap();
    }
    let leader = reader.string(flexible, "leader").unwrap().to_owned();
    let member = reader.string(flexible, "member").unwrap().to_owned();
    let count = reader.array_len(flexible, "members").unwrap().unwrap();
    let mut members = Vec::new();
    for _ in 0..count {
        members.push(reader.string(flexible, "id").unwrap().to_owned());
        if version >= 5 {
            reader.nullable_string(flexible, "instance").unwrap();
        }
        assert_eq!(reader.bytes(flexible, "metadata").unwrap(), subscription);
        if flexible {
            reader.tagged_fields("member").unwrap();
        }
    }
    Joined {
        code,
        generation,
        leader,
        member,
        members,
    }
}

/// Join as a first-time v7 member: told an id, then joined with it.
pub(super) async fn join_new(stream: &mut TcpStream, group: &str) -> Joined {
    let told = join(stream, 7, group, "").await;
    assert_eq!(told.code, 79, "MEMBER_ID_REQUIRED on a first join");
    assert!(told.member.starts_with("test-"), "{}", told.member);
    join(stream, 7, group, &told.member).await
}

async fn sync(
    stream: &mut TcpStream,
    group: &str,
    generation: i32,
    member: &str,
    assignments: &[(&str, &[u8])],
) -> (i16, Vec<u8>) {
    let mut body = Writer::new();
    body.string(true, group).unwrap();
    body.i32(generation);
    body.string(true, member).unwrap();
    body.nullable_string(true, None).unwrap();
    body.nullable_string(true, Some("consumer")).unwrap();
    body.nullable_string(true, Some("range")).unwrap();
    body.array_len(true, Some(assignments.len())).unwrap();
    for (id, assignment) in assignments {
        body.string(true, id).unwrap();
        body.bytes(true, assignment).unwrap();
        body.no_tagged_fields();
    }
    body.no_tagged_fields();
    let answer = ask(stream, 14, 5, true, &body.into_bytes()).await;
    let mut reader = Reader::new(&answer);
    reader.i32("throttle").unwrap();
    let code = reader.i16("error").unwrap();
    reader.nullable_string(true, "type").unwrap();
    reader.nullable_string(true, "protocol").unwrap();
    (code, reader.bytes(true, "assignment").unwrap().to_vec())
}

/// A body naming a member's place: group, generation, member, no instance.
fn place(group: &str, generation: i32, member: &str) -> Writer {
    let mut body = Writer::new();
    body.string(true, group).unwrap();
    body.i32(generation);
    body.string(true, member).unwrap();
    body.nullable_string(true, None).unwrap();
    body
}

async fn heartbeat(stream: &mut TcpStream, group: &str, generation: i32, member: &str) -> i16 {
    let mut body = place(group, generation, member);
    body.no_tagged_fields();
    let answer = ask(stream, 12, 4, true, &body.into_bytes()).await;
    let mut reader = Reader::new(&answer);
    reader.i32("throttle").unwrap();
    reader.i16("error").unwrap()
}

async fn commit(
    stream: &mut TcpStream,
    group: &str,
    generation: i32,
    member: &str,
    topic: &str,
    offset: i64,
) -> i16 {
    let mut body = place(group, generation, member);
    body.array_len(true, Some(1)).unwrap();
    body.string(true, topic).unwrap();
    body.array_len(true, Some(1)).unwrap();
    body.i32(0);
    body.i64(offset);
    body.i32(-1);
    body.nullable_string(true, None).unwrap();
    body.no_tagged_fields();
    body.no_tagged_fields();
    body.no_tagged_fields();
    let answer = ask(stream, 8, 8, true, &body.into_bytes()).await;
    let mut reader = Reader::new(&answer);
    reader.i32("throttle").unwrap();
    reader.array_len(true, "topics").unwrap();
    reader.string(true, "name").unwrap();
    reader.array_len(true, "partitions").unwrap();
    reader.i32("index").unwrap();
    reader.i16("error").unwrap()
}

/// The committed offsets fetched: every topic answered with its first partition's offset and code.
async fn committed(
    stream: &mut TcpStream,
    group: &str,
    topic: Option<&str>,
) -> Vec<(String, i64, i16)> {
    let mut body = Writer::new();
    body.string(true, group).unwrap();
    match topic {
        Some(topic) => {
            body.array_len(true, Some(1)).unwrap();
            body.string(true, topic).unwrap();
            body.array_len(true, Some(1)).unwrap();
            body.i32(0);
            body.no_tagged_fields();
        }
        None => body.array_len(true, None).unwrap(),
    }
    body.bool(false);
    body.no_tagged_fields();
    let answer = ask(stream, 9, 7, true, &body.into_bytes()).await;
    let mut reader = Reader::new(&answer);
    reader.i32("throttle").unwrap();
    let count = reader.array_len(true, "topics").unwrap().unwrap();
    let mut found = Vec::new();
    for _ in 0..count {
        let name = reader.string(true, "name").unwrap().to_owned();
        reader.array_len(true, "partitions").unwrap();
        reader.i32("index").unwrap();
        let offset = reader.i64("offset").unwrap();
        reader.i32("epoch").unwrap();
        reader.nullable_string(true, "metadata").unwrap();
        let code = reader.i16("error").unwrap();
        reader.tagged_fields("partition").unwrap();
        reader.tagged_fields("topic").unwrap();
        found.push((name, offset, code));
    }
    found
}

/// A group's state and member count, as DescribeGroups v5 gives them.
async fn describe(stream: &mut TcpStream, group: &str) -> (String, usize) {
    let mut body = Writer::new();
    body.array_len(true, Some(1)).unwrap();
    body.string(true, group).unwrap();
    body.bool(false);
    body.no_tagged_fields();
    let answer = ask(stream, 15, 5, true, &body.into_bytes()).await;
    let mut reader = Reader::new(&answer);
    reader.i32("throttle").unwrap();
    reader.array_len(true, "groups").unwrap();
    assert_eq!(reader.i16("error").unwrap(), 0);
    assert_eq!(reader.string(true, "group").unwrap(), group);
    let state = reader.string(true, "state").unwrap().to_owned();
    reader.string(true, "type").unwrap();
    reader.string(true, "protocol").unwrap();
    let members = reader.array_len(true, "members").unwrap().unwrap();
    (state, members)
}

/// Every group ListGroups v4 names.
pub(super) async fn list(stream: &mut TcpStream) -> Vec<String> {
    let mut body = Writer::new();
    body.array_len(true, Some(0)).unwrap();
    body.no_tagged_fields();
    let answer = ask(stream, 16, 4, true, &body.into_bytes()).await;
    let mut reader = Reader::new(&answer);
    reader.i32("throttle").unwrap();
    assert_eq!(reader.i16("error").unwrap(), 0);
    let count = reader.array_len(true, "groups").unwrap().unwrap();
    let mut names = Vec::new();
    for _ in 0..count {
        names.push(reader.string(true, "group").unwrap().to_owned());
        reader.string(true, "type").unwrap();
        reader.string(true, "state").unwrap();
        reader.tagged_fields("listed").unwrap();
    }
    names
}

fn one_batch(values: &[&[u8]]) -> Vec<u8> {
    let outgoing: Vec<Outgoing<'_>> = values
        .iter()
        .map(|value| Outgoing {
            timestamp: 1,
            key: None,
            value: Some(value),
            headers: &[],
        })
        .collect();
    let mut batch = Writer::new();
    encode_batch(&mut batch, 0, &outgoing).unwrap();
    batch.into_bytes()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_lone_member_is_assigned_and_its_commit_is_the_topics_reader_position() {
    let (address, stop, db) = serving_with("").await;
    let mut stream = TcpStream::connect(&address).await.unwrap();
    let joined = join_new(&mut stream, "billing").await;
    assert_eq!((joined.code, joined.generation), (0, 1));
    assert_eq!(joined.leader, joined.member, "the only member leads");
    assert_eq!(joined.members, vec![joined.member.clone()]);
    let me = joined.member.as_str();
    assert_eq!(
        sync(&mut stream, "billing", 1, me, &[(me, b"orders-0")]).await,
        (0, b"orders-0".to_vec())
    );
    assert_eq!(heartbeat(&mut stream, "billing", 1, me).await, 0);
    assert_eq!(
        committed(&mut stream, "billing", Some("orders")).await,
        vec![("orders".to_owned(), -1, 0)],
        "nothing committed yet"
    );
    let batch = one_batch(&[b"a", b"b", b"c"]);
    assert_eq!(
        produced(&ask(&mut stream, 0, 9, true, &produce_body("orders", &batch)).await),
        (0, 0)
    );
    assert_eq!(commit(&mut stream, "billing", 1, me, "orders", 2).await, 0);
    assert_eq!(
        committed(&mut stream, "billing", Some("orders")).await,
        vec![("orders".to_owned(), 2, 0)]
    );
    assert_eq!(
        committed(&mut stream, "billing", None).await,
        vec![("orders".to_owned(), 2, 0)],
        "every topic the group has read"
    );
    // The commit is the store's own reader position, read back without the surface.
    let mut session = db.session();
    let info = session
        .run("USE NAMESPACE app; USE DATABASE main; INFO FOR TOPIC orders;")
        .await
        .unwrap();
    let text = format!("{:?}", info.last().unwrap());
    assert!(text.contains("kafka:billing"), "{text}");
    let next = session
        .run("USE NAMESPACE app; USE DATABASE main; READ FROM orders FOR CONSUMER 'kafka:billing' LIMIT 1;")
        .await
        .unwrap();
    let next = format!("{:?}", next.last().unwrap());
    assert!(
        next.contains("\"position\": Number(Integer(3))") && next.contains("Bytes([99])"),
        "offset 2 is position 3: {next}"
    );
    let (code, _, records) =
        fetched(&ask(&mut stream, 1, 12, true, &fetch_body("orders", 2)).await);
    assert_eq!(code, 0);
    assert!(
        !records.is_empty(),
        "a fetch from the committed offset reads on"
    );
    stop.cancel();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_second_member_rebalances_the_first_into_one_generation() {
    let (address, stop) = serving().await;
    let mut first = TcpStream::connect(&address).await.unwrap();
    let joined = join_new(&mut first, "workers").await;
    let a = joined.member.clone();
    assert_eq!(
        sync(&mut first, "workers", 1, &a, &[(&a, b"all")]).await.0,
        0
    );
    let mut second = TcpStream::connect(&address).await.unwrap();
    // A version-3 member is given its id with its first join, and waits in it.
    let waiting = tokio::spawn(async move { join(&mut second, 3, "workers", "").await });
    let mut rebalancing = 0;
    while rebalancing != 27 {
        rebalancing = heartbeat(&mut first, "workers", 1, &a).await;
        assert!(matches!(rebalancing, 0 | 27), "{rebalancing}");
    }
    let rejoined = join(&mut first, 7, "workers", &a).await;
    let b = waiting.await.unwrap();
    assert_eq!((rejoined.code, b.code), (0, 0));
    assert_eq!((rejoined.generation, b.generation), (2, 2));
    assert_eq!(rejoined.leader, a, "the leader stays the leader");
    assert_eq!(rejoined.members.len(), 2, "the leader is told of both");
    assert!(b.members.is_empty(), "a follower is told of nobody");
    assert_eq!(
        heartbeat(&mut first, "workers", 1, &a).await,
        22,
        "ILLEGAL_GENERATION"
    );
    assert_eq!(commit(&mut first, "workers", 7, &a, "orders", 0).await, 22);
    assert_eq!(
        commit(&mut first, "workers", 2, "nobody", "orders", 0).await,
        25
    );
    stop.cancel();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_group_left_is_empty_and_then_deleted_once() {
    let (address, stop) = serving().await;
    let mut stream = TcpStream::connect(&address).await.unwrap();
    let joined = join_new(&mut stream, "audit").await;
    let me = joined.member.clone();
    sync(&mut stream, "audit", 1, &me, &[(&me, b"x")]).await;
    assert_eq!(
        describe(&mut stream, "audit").await,
        ("Stable".to_owned(), 1)
    );
    assert_eq!(list(&mut stream).await, vec!["audit".to_owned()]);
    let mut body = Writer::new();
    body.string(true, "audit").unwrap();
    body.array_len(true, Some(1)).unwrap();
    body.string(true, &me).unwrap();
    body.nullable_string(true, None).unwrap();
    body.no_tagged_fields();
    body.no_tagged_fields();
    ask(&mut stream, 13, 4, true, &body.into_bytes()).await;
    assert_eq!(
        describe(&mut stream, "audit").await,
        ("Empty".to_owned(), 0)
    );
    let delete = |group: &str| {
        let mut body = Writer::new();
        body.array_len(true, Some(1)).unwrap();
        body.string(true, group).unwrap();
        body.no_tagged_fields();
        body.into_bytes()
    };
    let deleted = |answer: Vec<u8>| {
        let mut reader = Reader::new(&answer);
        reader.i32("throttle").unwrap();
        reader.array_len(true, "results").unwrap();
        reader.string(true, "group").unwrap();
        reader.i16("error").unwrap()
    };
    assert_eq!(
        deleted(ask(&mut stream, 42, 2, true, &delete("audit")).await),
        0
    );
    assert_eq!(
        deleted(ask(&mut stream, 42, 2, true, &delete("audit")).await),
        69,
        "GROUP_ID_NOT_FOUND"
    );
    assert_eq!(describe(&mut stream, "audit").await, ("Dead".to_owned(), 0));
    stop.cancel();
}
