//! Topics made and dropped from a client, the topic list across databases, and
//! groups kept apart by the user that signed in.

use tokio::net::TcpStream;

use super::groups::{join_new, join_with, list};
use super::{ask, serving_with};
use crate::codec::{Reader, Writer};

/// Create one topic with `partitions` and `configs`; its code.
pub(super) async fn create(
    stream: &mut TcpStream,
    name: &str,
    partitions: i32,
    configs: &[(&str, &str)],
    validate_only: bool,
) -> i16 {
    let mut body = Writer::new();
    body.array_len(true, Some(1)).unwrap();
    body.string(true, name).unwrap();
    body.i32(partitions);
    body.i16(-1);
    body.array_len(true, Some(0)).unwrap();
    body.array_len(true, Some(configs.len())).unwrap();
    for (key, value) in configs {
        body.string(true, key).unwrap();
        body.nullable_string(true, Some(value)).unwrap();
        body.no_tagged_fields();
    }
    body.no_tagged_fields();
    body.i32(5_000);
    body.bool(validate_only);
    body.no_tagged_fields();
    let answer = ask(stream, 19, 5, true, &body.into_bytes()).await;
    let mut reader = Reader::new(&answer);
    reader.i32("throttle").unwrap();
    reader.array_len(true, "topics").unwrap();
    assert_eq!(reader.string(true, "name").unwrap(), name);
    reader.i16("error").unwrap()
}

async fn drop_topic(stream: &mut TcpStream, name: &str) -> i16 {
    let mut body = Writer::new();
    body.array_len(true, Some(1)).unwrap();
    body.string(true, name).unwrap();
    body.i32(5_000);
    body.no_tagged_fields();
    let answer = ask(stream, 20, 4, true, &body.into_bytes()).await;
    let mut reader = Reader::new(&answer);
    reader.i32("throttle").unwrap();
    reader.array_len(true, "responses").unwrap();
    reader.string(true, "name").unwrap();
    reader.i16("error").unwrap()
}

/// Every topic name a Metadata v9 asking for all of them answers.
async fn every_topic(stream: &mut TcpStream) -> Vec<String> {
    let mut body = Writer::new();
    body.array_len(true, None).unwrap();
    body.bool(false);
    body.bool(false);
    body.bool(false);
    body.no_tagged_fields();
    let answer = ask(stream, 3, 9, true, &body.into_bytes()).await;
    let mut reader = Reader::new(&answer);
    reader.i32("throttle").unwrap();
    reader.array_len(true, "brokers").unwrap();
    reader.i32("node").unwrap();
    reader.string(true, "host").unwrap();
    reader.i32("port").unwrap();
    reader.nullable_string(true, "rack").unwrap();
    reader.tagged_fields("broker").unwrap();
    reader.nullable_string(true, "cluster").unwrap();
    reader.i32("controller").unwrap();
    let count = reader.array_len(true, "topics").unwrap().unwrap();
    let mut names = Vec::new();
    for _ in 0..count {
        assert_eq!(reader.i16("error").unwrap(), 0);
        names.push(reader.string(true, "name").unwrap().to_owned());
        reader.bool("internal").unwrap();
        let partitions = reader.array_len(true, "partitions").unwrap().unwrap();
        for _ in 0..partitions {
            reader.i16("error").unwrap();
            reader.i32("index").unwrap();
            reader.i32("leader").unwrap();
            reader.i32("epoch").unwrap();
            reader.array_len(true, "replicas").unwrap();
            reader.i32("replica").unwrap();
            reader.array_len(true, "isr").unwrap();
            reader.i32("isr").unwrap();
            reader.array_len(true, "offline").unwrap();
            reader.tagged_fields("partition").unwrap();
        }
        reader.i32("operations").unwrap();
        reader.tagged_fields("topic").unwrap();
    }
    names.sort();
    names
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_client_creates_and_drops_topics_and_lists_every_database() {
    let (address, stop, db) = serving_with(
        "USE NAMESPACE app; DEFINE DATABASE other; USE DATABASE other; DEFINE TOPIC logs;",
    )
    .await;
    let mut stream = TcpStream::connect(&address).await.unwrap();
    assert_eq!(
        every_topic(&mut stream).await,
        vec!["app.other.logs".to_owned(), "orders".to_owned()],
        "the bare database's topics are bare, every other database's qualified, none twice"
    );
    assert_eq!(
        create(&mut stream, "events", 1, &[("retention.ms", "60000")], true).await,
        0
    );
    assert_eq!(
        create(&mut stream, "orders", 1, &[], true).await,
        36,
        "exists, checked only"
    );
    // Above the store's ceiling of partitions (ADR-0131).
    assert_eq!(
        create(&mut stream, "events", 1_025, &[], false).await,
        37,
        "INVALID_PARTITIONS"
    );
    assert_eq!(
        create(
            &mut stream,
            "events",
            1,
            &[("cleanup.policy", "compact")],
            false
        )
        .await,
        40,
        "INVALID_CONFIG"
    );
    assert_eq!(
        create(&mut stream, "app.main.bad-name", 1, &[], false).await,
        17
    );
    assert_eq!(
        create(
            &mut stream,
            "events",
            -1,
            &[
                ("retention.ms", "60000"),
                ("max.message.bytes", "4096"),
                ("segment.bytes", "1")
            ],
            false
        )
        .await,
        0
    );
    assert_eq!(
        create(&mut stream, "app.main.events", 1, &[], false).await,
        36,
        "TOPIC_ALREADY_EXISTS"
    );
    let mut session = db.session();
    let defined = session
        .run("USE NAMESPACE app; USE DATABASE main; INFO FOR TABLE events;")
        .await
        .unwrap();
    let text = format!("{:?}", defined.last().unwrap());
    assert!(
        text.contains("DEFINE TOPIC events RETAIN 1m MAX BYTES 4096"),
        "{text}"
    );
    assert_eq!(
        every_topic(&mut stream).await,
        vec![
            "app.other.logs".to_owned(),
            "events".to_owned(),
            "orders".to_owned()
        ]
    );
    assert_eq!(drop_topic(&mut stream, "events").await, 0);
    assert_eq!(
        drop_topic(&mut stream, "events").await,
        3,
        "UNKNOWN_TOPIC_OR_PARTITION"
    );
    stop.cancel();
}

pub(super) async fn sign_in(stream: &mut TcpStream, user: &str, password: &str) {
    let mut body = Writer::new();
    body.string(false, "PLAIN").unwrap();
    let answer = ask(stream, 17, 1, false, &body.into_bytes()).await;
    assert_eq!(Reader::new(&answer).i16("error").unwrap(), 0);
    let mut token = vec![0_u8];
    token.extend_from_slice(user.as_bytes());
    token.push(0);
    token.extend_from_slice(password.as_bytes());
    let mut body = Writer::new();
    body.bytes(true, &token).unwrap();
    body.no_tagged_fields();
    let answer = ask(stream, 36, 2, true, &body.into_bytes()).await;
    assert_eq!(
        Reader::new(&answer).i16("error").unwrap(),
        0,
        "{user} signs in"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn one_users_groups_are_not_anothers() {
    let (address, stop, db) =
        serving_with("DEFINE USER root ROLE owner PASSWORD 'root secret';").await;
    let mut root = db.session();
    root.sign_in("root", "root secret").await.unwrap();
    root.run(
        "DEFINE USER ada ROLE editor PASSWORD 'ada secret'; \
         DEFINE USER bob ROLE editor PASSWORD 'bob secret';",
    )
    .await
    .unwrap();
    let mut stranger = TcpStream::connect(&address).await.unwrap();
    assert_eq!(
        join_with(&mut stranger, 7, "shared-name", "", b"s")
            .await
            .code,
        30,
        "GROUP_AUTHORIZATION_FAILED: a closed store's stranger makes no group"
    );
    let mut ada = TcpStream::connect(&address).await.unwrap();
    sign_in(&mut ada, "ada", "ada secret").await;
    let joined = join_new(&mut ada, "shared-name").await;
    assert_eq!(joined.code, 0);
    let mut bob = TcpStream::connect(&address).await.unwrap();
    sign_in(&mut bob, "bob", "bob secret").await;
    assert!(
        list(&mut bob).await.is_empty(),
        "bob is not shown ada's group"
    );
    let own = join_new(&mut bob, "shared-name").await;
    assert_eq!(
        (own.code, own.generation),
        (0, 1),
        "bob's group of the same name is his own"
    );
    assert_eq!(list(&mut ada).await, vec!["shared-name".to_owned()]);
    let huge = vec![7_u8; 64 * 1024 + 1];
    assert_eq!(
        join_with(&mut ada, 7, "big", "", &huge).await.code,
        42,
        "a subscription past the ceiling is not kept"
    );
    stop.cancel();
}
