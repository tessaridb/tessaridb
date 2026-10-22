//! The surface served on an in-memory store and spoken to with this crate's
//! own codec: what a producer writes, a fetch reads back byte for byte.

use std::sync::Arc;

use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::TcpStream;
use tokio_util::sync::CancellationToken;

use crate::codec::{Reader, Writer};
use crate::records::{Header, Outgoing, decode_batches, encode_batch};
use crate::surface::{Database, Node};

mod admin;
mod brokers;
mod carrying;
mod groups;
mod partitions;

async fn serving() -> (String, CancellationToken) {
    let (address, stop, _) = serving_with("").await;
    (address, stop)
}

/// A node over a fresh store holding `app.main.orders`, after `setup` ran on it.
async fn serving_with(setup: &str) -> (String, CancellationToken, Arc<tessaridb::Db>) {
    let db = Arc::new(tessaridb::Db::in_memory().await.unwrap());
    let mut session = db.session();
    session
        .run("DEFINE NAMESPACE app; USE NAMESPACE app; DEFINE DATABASE main; USE DATABASE main; DEFINE TOPIC orders;")
        .await
        .unwrap();
    if !setup.is_empty() {
        session.run(setup).await.unwrap();
    }
    drop(session);
    let served = Arc::clone(&db);
    let node = Node::bind(served, "127.0.0.1:0")
        .unwrap()
        .reading_bare_names_in(Database::parse("app.main").unwrap());
    let address = node.address().unwrap();
    let stop = CancellationToken::new();
    let stopping = stop.clone();
    tokio::spawn(async move { node.serve(stopping).await });
    (address, stop, db)
}

/// Send one request (header v2 for a flexible version) and read its answer's body.
async fn ask(
    stream: &mut TcpStream,
    key: i16,
    version: i16,
    flexible: bool,
    body: &[u8],
) -> Vec<u8> {
    let mut frame = Writer::new();
    frame.i16(key);
    frame.i16(version);
    frame.i32(7);
    frame.nullable_string(false, Some("test")).unwrap();
    if flexible {
        frame.no_tagged_fields();
    }
    frame.raw(body);
    let frame = frame.into_bytes();
    stream
        .write_all(&u32::try_from(frame.len()).unwrap().to_be_bytes())
        .await
        .unwrap();
    stream.write_all(&frame).await.unwrap();
    let mut size = [0_u8; 4];
    stream.read_exact(&mut size).await.unwrap();
    let mut answer = vec![0_u8; usize::try_from(u32::from_be_bytes(size)).unwrap()];
    stream.read_exact(&mut answer).await.unwrap();
    // The correlation id and the response header's tags.
    let mut reader = Reader::new(&answer);
    assert_eq!(reader.i32("correlation").unwrap(), 7);
    if flexible {
        reader.tagged_fields("header").unwrap();
    }
    reader.rest().to_vec()
}

fn produce_body(topic: &str, records: &[u8]) -> Vec<u8> {
    produce_body_at(topic, 0, records)
}

fn produce_body_at(topic: &str, partition: i32, records: &[u8]) -> Vec<u8> {
    let mut body = Writer::new();
    body.nullable_string(true, None).unwrap();
    body.i16(-1);
    body.i32(5_000);
    body.array_len(true, Some(1)).unwrap();
    body.string(true, topic).unwrap();
    body.array_len(true, Some(1)).unwrap();
    body.i32(partition);
    body.nullable_bytes(true, Some(records)).unwrap();
    body.no_tagged_fields();
    body.no_tagged_fields();
    body.no_tagged_fields();
    body.into_bytes()
}

/// The error code and base offset of the one partition a produce answered.
fn produced(answer: &[u8]) -> (i16, i64) {
    let mut reader = Reader::new(answer);
    reader.array_len(true, "topics").unwrap();
    reader.string(true, "name").unwrap();
    reader.array_len(true, "partitions").unwrap();
    reader.i32("index").unwrap();
    (
        reader.i16("error").unwrap(),
        reader.i64("base offset").unwrap(),
    )
}

fn fetch_body(topic: &str, offset: i64) -> Vec<u8> {
    fetch_body_at(topic, 0, offset)
}

fn fetch_body_at(topic: &str, partition: i32, offset: i64) -> Vec<u8> {
    let mut body = Writer::new();
    body.i32(-1);
    body.i32(0);
    body.i32(1);
    body.i32(1 << 20);
    body.i8(0);
    body.i32(0);
    body.i32(-1);
    body.array_len(true, Some(1)).unwrap();
    body.string(true, topic).unwrap();
    body.array_len(true, Some(1)).unwrap();
    body.i32(partition);
    body.i32(-1);
    body.i64(offset);
    body.i32(-1);
    body.i64(-1);
    body.i32(1 << 20);
    body.no_tagged_fields();
    body.no_tagged_fields();
    body.array_len(true, Some(0)).unwrap();
    body.string(true, "").unwrap();
    body.no_tagged_fields();
    body.into_bytes()
}

/// The error code, high watermark and records of the one partition a fetch answered.
fn fetched(answer: &[u8]) -> (i16, i64, Vec<u8>) {
    let mut reader = Reader::new(answer);
    reader.i32("throttle").unwrap();
    reader.i16("error").unwrap();
    reader.i32("session").unwrap();
    reader.array_len(true, "topics").unwrap();
    reader.string(true, "name").unwrap();
    reader.array_len(true, "partitions").unwrap();
    reader.i32("index").unwrap();
    let error = reader.i16("error").unwrap();
    let high = reader.i64("high watermark").unwrap();
    reader.i64("last stable").unwrap();
    reader.i64("log start").unwrap();
    reader.array_len(true, "aborted").unwrap();
    reader.i32("preferred replica").unwrap();
    let records = reader
        .nullable_bytes(true, "records")
        .unwrap()
        .unwrap_or_default()
        .to_vec();
    (error, high, records)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn what_a_producer_writes_a_fetch_reads_back_with_its_offsets() {
    let (address, stop) = serving().await;
    let mut stream = TcpStream::connect(&address).await.unwrap();
    let headers = [("trace", Some(&b"t-1"[..]))];
    let outgoing = [
        Outgoing {
            timestamp: 1_760_000_000_000,
            key: Some(b"k1"),
            value: Some(b"\x00binary\xff"),
            headers: &headers,
        },
        Outgoing {
            timestamp: 1_760_000_000_001,
            key: None,
            value: Some(b"second"),
            headers: &[],
        },
    ];
    let mut batch = Writer::new();
    encode_batch(&mut batch, 0, &outgoing).unwrap();
    let batch = batch.into_bytes();
    assert_eq!(
        produced(&ask(&mut stream, 0, 9, true, &produce_body("orders", &batch)).await),
        (0, 0)
    );
    assert_eq!(
        produced(
            &ask(
                &mut stream,
                0,
                9,
                true,
                &produce_body("app.main.orders", &batch)
            )
            .await
        ),
        (0, 2),
        "the fully qualified name reaches the same topic, after the first batch"
    );
    let (error, high, records) =
        fetched(&ask(&mut stream, 1, 12, true, &fetch_body("orders", 1)).await);
    assert_eq!((error, high), (0, 4));
    let batches = decode_batches(&records).unwrap();
    let read: Vec<_> = batches
        .iter()
        .flat_map(|batch| {
            batch.records.iter().enumerate().map(move |(at, record)| {
                (
                    batch.base_offset + i64::try_from(at).unwrap(),
                    record.clone(),
                )
            })
        })
        .collect();
    assert_eq!(read.len(), 3, "offsets 1, 2 and 3");
    assert_eq!(read[0].0, 1);
    assert_eq!(read[0].1.value.as_deref(), Some(&b"second"[..]));
    assert_eq!(read[1].0, 2);
    assert_eq!(read[1].1.key.as_deref(), Some(&b"k1"[..]));
    assert_eq!(read[1].1.value.as_deref(), Some(&b"\x00binary\xff"[..]));
    assert_eq!(
        read[1].1.headers,
        vec![Header {
            key: "trace".to_owned(),
            value: Some(b"t-1".to_vec())
        }]
    );
    assert_eq!(read[1].1.timestamp, 1_760_000_000_000);
    stop.cancel();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_topic_the_store_does_not_hold_is_unknown_and_an_offset_past_the_end_is_out_of_range() {
    let (address, stop) = serving().await;
    let mut stream = TcpStream::connect(&address).await.unwrap();
    let mut batch = Writer::new();
    encode_batch(
        &mut batch,
        0,
        &[Outgoing {
            timestamp: 1,
            key: None,
            value: Some(b"x"),
            headers: &[],
        }],
    )
    .unwrap();
    let batch = batch.into_bytes();
    for missing in [
        "nothing",
        "app.main.nothing",
        "app.elsewhere.orders",
        "app.main.or-ders",
    ] {
        let (error, _) =
            produced(&ask(&mut stream, 0, 9, true, &produce_body(missing, &batch)).await);
        assert_eq!(error, 3, "{missing}: UNKNOWN_TOPIC_OR_PARTITION");
    }
    let (error, _, _) = fetched(&ask(&mut stream, 1, 12, true, &fetch_body("orders", 5)).await);
    assert_eq!(error, 1, "OFFSET_OUT_OF_RANGE past an empty topic's end");
    stop.cancel();
}

/// A message appended by a statement carries the instant it was appended as
/// its Kafka timestamp — the log's own time — rather than none (ADR-0133 D3).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_message_a_statement_appended_carries_its_append_time() {
    let wall = || {
        i64::try_from(
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_millis(),
        )
        .unwrap()
    };
    let before = wall();
    let (address, stop, _) = serving_with("CREATE orders:'n1' = { amount: 5 };").await;
    let after = wall();
    let mut stream = TcpStream::connect(&address).await.unwrap();
    let (error, _, records) =
        fetched(&ask(&mut stream, 1, 12, true, &fetch_body("orders", 0)).await);
    assert_eq!(error, 0);
    let batches = decode_batches(&records).unwrap();
    let timestamp = batches[0].records[0].timestamp;
    assert!(
        before <= timestamp && timestamp <= after,
        "{before} <= {timestamp} <= {after}"
    );
    stop.cancel();
}

/// ListOffsets by time finds a statement-appended message by the instant it
/// was appended, the timestamp a fetch reports for it (ADR-0133 D3).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn offsets_for_a_time_find_messages_a_statement_appended() {
    let (address, stop, _) = serving_with(
        "CREATE orders:'n1' = { amount: 1 }; CREATE orders:'n2' = { amount: 2 }; \
         CREATE orders:'n3' = { amount: 3 };",
    )
    .await;
    let mut stream = TcpStream::connect(&address).await.unwrap();
    let (_, _, records) = fetched(&ask(&mut stream, 1, 12, true, &fetch_body("orders", 0)).await);
    let stamps: Vec<i64> = decode_batches(&records)
        .unwrap()
        .iter()
        .flat_map(|batch| batch.records.iter().map(|record| record.timestamp))
        .collect();
    assert_eq!(stamps.len(), 3);
    let target = stamps[2];
    let expected = stamps.iter().position(|stamp| *stamp >= target).unwrap();
    let mut body = Writer::new();
    body.i32(-1);
    body.array_len(false, Some(1)).unwrap();
    body.string(false, "orders").unwrap();
    body.array_len(false, Some(1)).unwrap();
    body.i32(0);
    body.i64(target);
    let answer = ask(&mut stream, 2, 1, false, &body.into_bytes()).await;
    let mut reader = Reader::new(&answer);
    reader.array_len(false, "topics").unwrap();
    reader.string(false, "name").unwrap();
    reader.array_len(false, "partitions").unwrap();
    reader.i32("index").unwrap();
    assert_eq!(reader.i16("error").unwrap(), 0);
    assert_eq!(reader.i64("timestamp").unwrap(), stamps[expected]);
    assert_eq!(
        reader.i64("offset").unwrap(),
        i64::try_from(expected).unwrap()
    );
    stop.cancel();
}
